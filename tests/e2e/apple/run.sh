#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
apple_root="${repo_root}/travel-apple/FlowSpliceTravel"
compose_file="${repo_root}/tests/e2e/compose.yaml"
password_file="${1:?Apple Travel private-key password file is required}"
relay_address="${2:-127.0.0.1:18446}"
simulator_name="${3:-iPhone 17 Pro}"
travel_id="${4:-apple-e2e-travel}"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

if [[ ! -s "${password_file}" ]]; then
  echo 'Apple Travel E2E password input is missing' >&2
  exit 1
fi
if [[ ! -d "${DEVELOPER_DIR}" ]]; then
  echo "Xcode developer directory was not found: ${DEVELOPER_DIR}" >&2
  exit 1
fi

simulator_id="$(
  xcrun simctl list devices available -j |
    python3 -c '
import json, sys
name = sys.argv[1]
devices = json.load(sys.stdin)["devices"]
matches = [item for runtime in devices.values() for item in runtime if item.get("name") == name and item.get("isAvailable", True)]
if not matches:
    raise SystemExit(f"No available simulator named {name!r}")
booted = next((item for item in matches if item.get("state") == "Booted"), matches[0])
print(booted["udid"])
' "${simulator_name}"
)"

xcrun simctl shutdown "${simulator_id}" >/dev/null 2>&1 || true
xcrun simctl boot "${simulator_id}"
xcrun simctl bootstatus "${simulator_id}" -b
xcrun simctl io "${simulator_id}" screenConfig power on >/dev/null
xcrun simctl terminate "${simulator_id}" io.zxf.flowsplice.travel >/dev/null 2>&1 || true
xcrun simctl uninstall "${simulator_id}" io.zxf.flowsplice.travel >/dev/null 2>&1 || true

private_key_password="$(tr -d '\r\n' <"${password_file}")"
test_output="$(mktemp -t flowsplice-apple-e2e.XXXXXX)"
derived_data="$(mktemp -d -t flowsplice-apple-derived.XXXXXX)"
test_pid=''
test_succeeded=0
cleanup() {
  if [[ -n "${test_pid}" ]] && kill -0 "${test_pid}" 2>/dev/null; then
    kill "${test_pid}" 2>/dev/null || true
  fi
  xcrun simctl io "${simulator_id}" screenConfig power on >/dev/null 2>&1 || true
  xcrun simctl terminate "${simulator_id}" io.zxf.flowsplice.travel >/dev/null 2>&1 || true
  if [[ "${test_succeeded}" != "1" && -s "${test_output}" ]]; then
    echo "Apple E2E failure log: ${test_output}" >&2
    sed -n '1,360p' "${test_output}" >&2
  else
    rm -f "${test_output}"
  fi
  rm -rf "${derived_data}"
}
trap cleanup EXIT

xcodebuild \
  -project "${apple_root}/FlowSpliceTravel.xcodeproj" \
  -scheme FlowSpliceTravel \
  -destination "platform=iOS Simulator,id=${simulator_id}" \
  -derivedDataPath "${derived_data}" \
  -parallel-testing-enabled NO \
  build-for-testing >"${test_output}" 2>&1
xctestrun_file="$(find "${derived_data}/Build/Products" -maxdepth 1 -name '*.xctestrun' -print -quit)"
if [[ -z "${xctestrun_file}" ]]; then
  echo 'Xcode did not produce an xctestrun file' >&2
  exit 1
fi
environment_path='TestConfigurations.0.TestTargets.0.EnvironmentVariables'
testing_environment_path='TestConfigurations.0.TestTargets.0.TestingEnvironmentVariables'
plutil -insert "${environment_path}.FLOWSPLICE_APPLE_E2E" -string "1" "${xctestrun_file}"
plutil -insert "${environment_path}.FLOWSPLICE_APPLE_E2E_RELAY" -string "${relay_address}" "${xctestrun_file}"
plutil -insert "${environment_path}.FLOWSPLICE_APPLE_E2E_PASSWORD" -string "${private_key_password}" "${xctestrun_file}"
plutil -insert "${environment_path}.FLOWSPLICE_APPLE_E2E_TRAVEL_ID" -string "${travel_id}" "${xctestrun_file}"
plutil -insert "${testing_environment_path}.FLOWSPLICE_APPLE_E2E" -string "1" "${xctestrun_file}"
plutil -insert "${testing_environment_path}.FLOWSPLICE_APPLE_E2E_RELAY" -string "${relay_address}" "${xctestrun_file}"
plutil -insert "${testing_environment_path}.FLOWSPLICE_APPLE_E2E_PASSWORD" -string "${private_key_password}" "${xctestrun_file}"
plutil -insert "${testing_environment_path}.FLOWSPLICE_APPLE_E2E_TRAVEL_ID" -string "${travel_id}" "${xctestrun_file}"
plutil -insert 'TestConfigurations.0.TestTargets.0.CommandLineArguments.0' -string '--flowsplice-apple-e2e' "${xctestrun_file}"
plutil -replace 'TestConfigurations.0.TestTargets.0.ParallelizationEnabled' -bool NO "${xctestrun_file}"
xcodebuild \
  -xctestrun "${xctestrun_file}" \
  -destination "platform=iOS Simulator,id=${simulator_id}" \
  -derivedDataPath "${derived_data}" \
  -parallel-testing-enabled NO \
  -only-testing:FlowSpliceTravelUITests/FlowSpliceTravelUITests/testDockerRemoteEnrollmentCatalogMappingAndRecovery \
  test-without-building >>"${test_output}" 2>&1 &
test_pid=$!

pending="$(python3 "${repo_root}/tests/e2e/home-issuer-client.py" pending \
  --port 19084 \
  --travel-id "${travel_id}" \
  --wait-secs 180)"
request_id="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["request_id"])' "${pending}")"
home_verification_code="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["verification_code"])' "${pending}")"

app_container=''
apple_verification_code=''
for _ in $(seq 1 60); do
  app_container="$(xcrun simctl get_app_container "${simulator_id}" io.zxf.flowsplice.travel data 2>/dev/null || true)"
  if [[ -n "${app_container}" && -s "${app_container}/Documents/e2e-verification-code" ]]; then
    apple_verification_code="$(tr -d '\r\n' <"${app_container}/Documents/e2e-verification-code")"
    break
  fi
  sleep 1
done
if [[ "${apple_verification_code}" != "${home_verification_code}" ]]; then
  echo 'Apple Travel and Home verification codes did not match' >&2
  sed -n '1,260p' "${test_output}" >&2
  exit 1
fi
python3 "${repo_root}/tests/e2e/home-issuer-client.py" approve \
  --port 19084 \
  --request-id "${request_id}" \
  --password-file "${password_file}" \
  --scope global \
  --valid-days 1 >/dev/null

wait_for_app_phase() {
  local phase="$1"
  local phase_path="${app_container}/Documents/e2e-phase-${phase}"
  for _ in $(seq 1 180); do
    if [[ -s "${phase_path}" ]]; then
      return 0
    fi
    if ! kill -0 "${test_pid}" 2>/dev/null; then
      echo "Apple E2E ended before phase ${phase}" >&2
      sed -n '1,320p' "${test_output}" >&2
      return 1
    fi
    sleep 1
  done
  echo "Timed out waiting for Apple E2E phase ${phase}" >&2
  return 1
}

wait_for_app_phase network-outage-ready
docker compose -f "${compose_file}" stop relay1 >/dev/null
sleep 5
docker compose -f "${compose_file}" start relay1 >/dev/null
for _ in $(seq 1 30); do
  if nc -z 127.0.0.1 18446 2>/dev/null && nc -z 127.0.0.1 18447 2>/dev/null; then
    break
  fi
  sleep 1
done

wait_for_app_phase screen-off-ready
xcrun simctl io "${simulator_id}" screenConfig power off
sleep 5
xcrun simctl io "${simulator_id}" screenConfig power on

set +e
wait "${test_pid}"
test_status=$?
set -e
test_pid=''
sed -n '1,360p' "${test_output}"
if [[ "${test_status}" -ne 0 ]] || ! grep -Fq 'FLOWSPLICE_APPLE_E2E_COMPLETE' "${test_output}"; then
  echo "Apple Travel UI E2E did not pass on ${simulator_name}" >&2
  exit 1
fi
test_succeeded=1
printf '%s\n' "{\"checkpoint\":\"apple-remote-enrollment-catalog-mapping-network-screen-off-restart\",\"simulator\":\"${simulator_name}\"}"
