#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
mac_root="${repo_root}/travel-macos/FlowSpliceMac"
compose_file="${repo_root}/tests/e2e/compose.yaml"
password_file="${1:?macOS Travel private-key password file is required}"
relay_address="${2:-127.0.0.1:18446}"
travel_id="${3:-macos-e2e-travel}"
local_port="${FLOWSPLICE_MACOS_E2E_LOCAL_PORT:-10800}"
test_mode="${FLOWSPLICE_MACOS_TEST_MODE:-remote}"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

case "${test_mode}" in
  remote | local) ;;
  *)
    echo 'FLOWSPLICE_MACOS_TEST_MODE must be remote or local' >&2
    exit 1
    ;;
esac

deployment_root_input=''
if [[ "${test_mode}" == 'remote' ]]; then
  deployment_root_input="${FLOWSPLICE_E2E_DEPLOYMENT_ROOT_FILE:-${repo_root}/tests/e2e/generated/certs/deployment-root.pub}"
fi

if [[ ! -s "${password_file}" ]]; then
  echo 'macOS Travel E2E password input is missing' >&2
  exit 1
fi
if [[ ! -d "${DEVELOPER_DIR}" ]]; then
  echo "Xcode developer directory was not found: ${DEVELOPER_DIR}" >&2
  exit 1
fi
if [[ "${test_mode}" == 'remote' ]] && \
   [[ ! -f "${deployment_root_input}" || ! -r "${deployment_root_input}" || ! -s "${deployment_root_input}" ]]; then
  echo 'macOS Travel E2E deployment-root test input is missing or unreadable' >&2
  exit 1
fi

private_key_password="$(tr -d '\r\n' <"${password_file}")"
test_output="$(mktemp -t flowsplice-macos-e2e.XXXXXX)"
runtime_log="$(mktemp -t flowsplice-macos-runtime.XXXXXX)"
derived_data="$(mktemp -d -t flowsplice-macos-derived.XXXXXX)"
if command -v uuidgen >/dev/null 2>&1; then
  marker_token="$(uuidgen | tr -d '-')"
else
  marker_token="$$-$(date +%s)"
fi
ui_test_runner_container="${HOME}/Library/Containers/io.zxf.flowsplice.travel.FlowSpliceMacUITests.xctrunner/Data"
marker_dir="${ui_test_runner_container}/Library/Caches/FlowSpliceMacE2E/${marker_token}"
keychain_dir="$(mktemp -d -t flowsplice-macos-keychain.XXXXXX)"
ui_keychain_path="${keychain_dir}/flowsplice-ui-testing.keychain-db"
if [[ -z "${FLOWSPLICE_UI_TEST_KEYCHAIN_PASSWORD:-}" ]]; then
  if command -v uuidgen >/dev/null 2>&1; then
    ui_keychain_password="$(uuidgen | tr -d '-')"
  else
    ui_keychain_password="flowsplice-ui-$(date +%s)"
  fi
else
  ui_keychain_password="${FLOWSPLICE_UI_TEST_KEYCHAIN_PASSWORD}"
fi
keychain_state_before="$(mktemp -t flowsplice-macos-keychain-before.XXXXXX)"
keychain_state_after="$(mktemp -t flowsplice-macos-keychain-after.XXXXXX)"
keychain_state_diff="$(mktemp -t flowsplice-macos-keychain-diff.XXXXXX)"
test_pid=''
log_pid=''
test_succeeded=0

capture_keychain_state() {
  security default-keychain
  security list-keychains
}

cleanup() {
  if [[ -f "${keychain_state_before}" ]]; then
    rm -f "${keychain_state_before}"
  fi
  if [[ -f "${keychain_state_after}" ]]; then
    rm -f "${keychain_state_after}"
  fi
  if [[ -f "${keychain_state_diff}" ]]; then
    rm -f "${keychain_state_diff}"
  fi
  if [[ -e "${ui_keychain_path}" ]]; then
    security delete-keychain "${ui_keychain_path}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${test_pid}" ]] && kill -0 "${test_pid}" 2>/dev/null; then
    kill "${test_pid}" 2>/dev/null || true
    wait "${test_pid}" 2>/dev/null || true
  fi
  if [[ -n "${log_pid}" ]] && kill -0 "${log_pid}" 2>/dev/null; then
    kill "${log_pid}" 2>/dev/null || true
    wait "${log_pid}" 2>/dev/null || true
  fi
  if [[ "${test_succeeded}" != "1" && -s "${test_output}" ]]; then
    echo "macOS E2E failure log: ${test_output}" >&2
    sed -n '1,420p' "${test_output}" >&2
    if [[ -s "${runtime_log}" ]]; then
      echo "macOS runtime log: ${runtime_log}" >&2
      tail -n 240 "${runtime_log}" >&2
    fi
  else
    rm -f "${test_output}"
    rm -f "${runtime_log}"
  fi
  rm -rf -- "${derived_data}" "${keychain_dir}" "${marker_dir}"
}
trap cleanup EXIT

trust_xcode_args=(
  'FLOWSPLICE_PRIVATE_TRUST_FILE='
  "FLOWSPLICE_TRUST_INPUT_LIST=${repo_root}/scripts/empty-trust-inputs.xcfilelist"
)
frozen_deployment_root=''
if [[ "${test_mode}" == 'remote' ]]; then
  trust_directory="${derived_data}/trust"
  staged_deployment_root="${trust_directory}/deployment-root-input.pub"
  frozen_deployment_root="${trust_directory}/deployment-root.pub"
  trust_input_list="${trust_directory}/trust-inputs.xcfilelist"
  mkdir -p "${trust_directory}"
  cp "${deployment_root_input}" "${staged_deployment_root}"
  chmod 600 "${staged_deployment_root}"
  python3 "${repo_root}/scripts/private-travel-trust.py" copy-root \
    --source "${staged_deployment_root}" --destination "${frozen_deployment_root}"
  printf '%s\n' "${frozen_deployment_root}" >"${trust_input_list}"
  trust_xcode_args=(
    "FLOWSPLICE_PRIVATE_TRUST_FILE=${frozen_deployment_root}"
    "FLOWSPLICE_TRUST_INPUT_LIST=${trust_input_list}"
  )
else
  unset FLOWSPLICE_PRIVATE_TRUST_FILE FLOWSPLICE_TRUST_INPUT_LIST
fi

capture_keychain_state >"${keychain_state_before}"
security create-keychain -p "${ui_keychain_password}" "${ui_keychain_path}" >/dev/null
security unlock-keychain -p "${ui_keychain_password}" "${ui_keychain_path}" >/dev/null

xcodebuild \
  -project "${mac_root}/FlowSpliceMac.xcodeproj" \
  -scheme FlowSpliceMac \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath "${derived_data}" \
  -parallel-testing-enabled NO \
  "${trust_xcode_args[@]}" \
  build-for-testing >"${test_output}" 2>&1

mac_bundle="$(find "${derived_data}/Build/Products" -type d -name 'FlowSpliceMac.app' -print -quit)"
if [[ -z "${mac_bundle}" ]]; then
  echo 'Xcode did not produce the macOS Travel app bundle for deployment-root verification' >&2
  exit 1
fi
if [[ "${test_mode}" == 'remote' ]]; then
  python3 "${repo_root}/scripts/private-travel-trust.py" verify-root \
    --source "${frozen_deployment_root}" \
    --artifact-resource "${mac_bundle}/Contents/Resources/bootstrap/deployment-root.pub"
elif [[ -e "${mac_bundle}/Contents/Resources/bootstrap/deployment-root.pub" ]]; then
  echo 'macOS local E2E unexpectedly bundled a deployment root' >&2
  exit 1
fi

xctestrun_file="$(find "${derived_data}/Build/Products" -maxdepth 1 -name '*.xctestrun' -print -quit)"
if [[ -z "${xctestrun_file}" ]]; then
  echo 'Xcode did not produce a macOS xctestrun file' >&2
  exit 1
fi

ui_test_target_index=''
candidate_index=0
while blueprint_name="$(
  plutil -extract "TestConfigurations.0.TestTargets.${candidate_index}.BlueprintName" \
    raw "${xctestrun_file}" 2>/dev/null
)"; do
  if [[ "${blueprint_name}" == 'FlowSpliceMacUITests' ]]; then
    ui_test_target_index="${candidate_index}"
    break
  fi
  candidate_index="$((candidate_index + 1))"
done
if [[ -z "${ui_test_target_index}" ]]; then
  echo 'Xcode test configuration did not contain FlowSpliceMacUITests' >&2
  exit 1
fi

test_target_path="TestConfigurations.0.TestTargets.${ui_test_target_index}"
environment_path="${test_target_path}.EnvironmentVariables"
testing_environment_path="${test_target_path}.TestingEnvironmentVariables"
for target_environment in "${environment_path}" "${testing_environment_path}"; do
  if [[ "${test_mode}" == 'remote' ]]; then
    plutil -insert "${target_environment}.FLOWSPLICE_MACOS_E2E" -string '1' "${xctestrun_file}"
    plutil -insert "${target_environment}.FLOWSPLICE_MACOS_E2E_RELAY" -string "${relay_address}" "${xctestrun_file}"
    plutil -insert "${target_environment}.FLOWSPLICE_MACOS_E2E_PASSWORD" -string "${private_key_password}" "${xctestrun_file}"
    plutil -insert "${target_environment}.FLOWSPLICE_MACOS_E2E_TRAVEL_ID" -string "${travel_id}" "${xctestrun_file}"
    plutil -insert "${target_environment}.FLOWSPLICE_MACOS_E2E_LOCAL_PORT" -string "${local_port}" "${xctestrun_file}"
    plutil -insert "${target_environment}.FLOWSPLICE_MACOS_E2E_MARKER_TOKEN" -string "${marker_token}" "${xctestrun_file}"
  fi
  plutil -insert "${target_environment}.FLOWSPLICE_UI_TEST_KEYCHAIN_PATH" -string "${ui_keychain_path}" "${xctestrun_file}"
  plutil -insert "${target_environment}.FLOWSPLICE_UI_TEST_KEYCHAIN_PASSWORD" -string "${ui_keychain_password}" "${xctestrun_file}"
done
plutil -replace "${test_target_path}.ParallelizationEnabled" -bool NO "${xctestrun_file}"

/usr/bin/log stream \
  --style compact \
  --level info \
  --predicate 'subsystem == "io.zxf.flowsplice.travel.macos"' \
  >"${runtime_log}" 2>&1 &
log_pid=$!

if [[ "${test_mode}" == 'local' ]]; then
  local_tests=(
    '-only-testing:FlowSpliceMacTests'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testEnrollmentValidationAndCompletion'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testControlCenterMappingsDiagnosticsAndLifecycle'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testMenuBarIconUsesNativeStatusItemFootprint'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testCloseButtonHidesButDoesNotTerminate'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testCommandQHidesButDoesNotTerminate'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testDockQuitHidesButDoesNotTerminate'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testStatusBarRealQuitShowsCancelAndConfirm'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testStatusBarRealQuitConfirmsWithNoActiveFlows'
    '-only-testing:FlowSpliceMacUITests/FlowSpliceMacUITestsLaunchTests/testLaunchVisual'
  )
  set +e
  xcodebuild \
    -xctestrun "${xctestrun_file}" \
    -destination 'platform=macOS,arch=arm64' \
    -derivedDataPath "${derived_data}" \
    -parallel-testing-enabled NO \
    "${local_tests[@]}" \
    test-without-building >>"${test_output}" 2>&1
  test_status=$?
  set -e
  if [[ -n "${log_pid}" ]] && kill -0 "${log_pid}" 2>/dev/null; then
    kill "${log_pid}" 2>/dev/null || true
    wait "${log_pid}" 2>/dev/null || true
  fi
  log_pid=''
  sed -n '1,520p' "${test_output}"
  if [[ "${test_status}" != '0' ]]; then
    exit "${test_status}"
  fi
  if grep -Eq "Test Case .* skipped|[1-9][0-9]* test(s)? skipped" "${test_output}"; then
    echo 'macOS local native suite skipped a selected test' >&2
    exit 1
  fi
  capture_keychain_state >"${keychain_state_after}"
  if ! diff -u "${keychain_state_before}" "${keychain_state_after}" >"${keychain_state_diff}"; then
    echo 'macOS keychain state changed during UI tests; this should not happen.' >&2
    sed -n '1,260p' "${keychain_state_diff}" >&2
    exit 1
  fi
  printf '%s\n' '{"checkpoint":"macos-native-local-suite-complete"}'
  test_succeeded=1
  exit 0
fi

xcodebuild \
  -xctestrun "${xctestrun_file}" \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath "${derived_data}" \
  -parallel-testing-enabled NO \
  -only-testing:FlowSpliceMacUITests/FlowSpliceMacUITests/testDockerRemoteEnrollmentMappingDiagnosticsAndRecovery \
  test-without-building >>"${test_output}" 2>&1 &
test_pid=$!

for _ in $(seq 1 90); do
  if [[ -s "${marker_dir}/verification-code" ]]; then
    break
  fi
  if ! kill -0 "${test_pid}" 2>/dev/null; then
    echo 'macOS E2E ended before exposing its verification code' >&2
    exit 1
  fi
  sleep 1
done
if [[ ! -s "${marker_dir}/verification-code" ]]; then
  echo 'macOS E2E did not expose a verification code' >&2
  exit 1
fi

pending="$(python3 "${repo_root}/tests/e2e/home-issuer-client.py" pending \
  --port 19084 \
  --travel-id "${travel_id}" \
  --wait-secs 30)"
request_id="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["request_id"])' "${pending}")"
home_verification_code="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["verification_code"])' "${pending}")"
mac_verification_code="$(tr -d '\r\n' <"${marker_dir}/verification-code")"
if [[ "${mac_verification_code}" != "${home_verification_code}" ]]; then
  echo 'macOS Travel and Home verification codes did not match' >&2
  exit 1
fi

python3 "${repo_root}/tests/e2e/home-issuer-client.py" approve \
  --port 19084 \
  --request-id "${request_id}" \
  --password-file "${password_file}" \
  --scope global \
  --valid-days 1 >/dev/null

for _ in $(seq 1 180); do
  if [[ -s "${marker_dir}/network-outage-ready" && -s "${marker_dir}/selected-relay" ]]; then
    break
  fi
  if ! kill -0 "${test_pid}" 2>/dev/null; then
    echo 'macOS E2E ended before the Relay outage phase' >&2
    exit 1
  fi
  sleep 1
done
if [[ ! -s "${marker_dir}/network-outage-ready" || ! -s "${marker_dir}/selected-relay" ]]; then
  echo 'macOS E2E did not reach the Relay outage phase' >&2
  exit 1
fi

selected_relay="$(tr -d '\r\n' <"${marker_dir}/selected-relay")"
case "${selected_relay}" in
  relay-1)
    relay_service='relay1'
    relay_management_port=18446
    relay_data_port=18447
    ;;
  relay-2)
    relay_service='relay2'
    relay_management_port=28446
    relay_data_port=28447
    ;;
  *)
    echo "macOS E2E selected an unknown Relay: ${selected_relay}" >&2
    exit 1
    ;;
esac

docker compose -f "${compose_file}" stop "${relay_service}" >/dev/null
sleep 5
docker compose -f "${compose_file}" start "${relay_service}" >/dev/null
for _ in $(seq 1 60); do
  if nc -z 127.0.0.1 "${relay_management_port}" 2>/dev/null && \
     nc -z 127.0.0.1 "${relay_data_port}" 2>/dev/null; then
    break
  fi
  sleep 1
done
if ! nc -z 127.0.0.1 "${relay_management_port}" 2>/dev/null || \
   ! nc -z 127.0.0.1 "${relay_data_port}" 2>/dev/null; then
  echo "${selected_relay} did not recover after restart" >&2
  exit 1
fi
printf 'complete\n' >"${marker_dir}/network-outage-complete"

set +e
wait "${test_pid}"
test_status=$?
set -e
test_pid=''
if [[ -n "${log_pid}" ]] && kill -0 "${log_pid}" 2>/dev/null; then
  kill "${log_pid}" 2>/dev/null || true
  wait "${log_pid}" 2>/dev/null || true
fi
log_pid=''
sed -n '1,420p' "${test_output}"
if [[ "${test_status}" != "0" ]]; then
  exit "${test_status}"
fi
if [[ ! -s "${marker_dir}/complete" ]]; then
  echo 'macOS E2E did not write its completion marker' >&2
  exit 1
fi
capture_keychain_state >"${keychain_state_after}"
if ! diff -u "${keychain_state_before}" "${keychain_state_after}" >"${keychain_state_diff}"; then
  echo 'macOS keychain state changed during UI tests; this should not happen.' >&2
  sed -n '1,260p' "${keychain_state_diff}" >&2
  exit 1
fi

printf '%s\n' '{"checkpoint":"macos-native-e2e-complete"}'
test_succeeded=1
