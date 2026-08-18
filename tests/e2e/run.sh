#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
compose_file="${repo_root}/tests/e2e/compose.yaml"
generated_dir="${repo_root}/tests/e2e/generated"
log_file="${generated_dir}/e2e.log"
export FLOWSPLICE_E2E_UID="$(id -u)"
export FLOWSPLICE_E2E_GID="$(id -g)"

mkdir -p "${generated_dir}"

docker build -f "${repo_root}/docker/e2e.Dockerfile" -t flowsplice-e2e:local "${repo_root}"
"${repo_root}/tests/e2e/generate-certs.sh"

teardown() {
  docker compose -f "${compose_file}" down --volumes --remove-orphans >/dev/null 2>&1 || true
}

finish() {
  status=$?
  set +e
  docker compose -f "${compose_file}" logs --no-color >"${log_file}" 2>&1
  if (( status != 0 )); then
    tail -n 300 "${log_file}" >&2
  fi
  teardown
  trap - EXIT
  exit "${status}"
}

teardown
trap finish EXIT
docker compose -f "${compose_file}" up -d echo echo2 relay1 relay2 server homeagent homeagent2

home_issuer_ready=0
for _ in $(seq 1 60); do
  if python3 "${repo_root}/tests/e2e/home-issuer-client.py" status \
    --port 19081 >/dev/null 2>&1; then
    home_issuer_ready=1
    break
  fi
  sleep 1
done
if (( home_issuer_ready == 0 )); then
  echo 'Home issuer UI did not become ready' >&2
  exit 1
fi

printf '%s\n' 'wrong-flowsplice-e2e-password' >"${generated_dir}/offline/wrong-password.txt"
python3 "${repo_root}/tests/e2e/home-issuer-client.py" issue \
  --port 19081 \
  --request "${generated_dir}/travel/enrollment-request.json" \
  --password-file "${generated_dir}/offline/wrong-password.txt" \
  --scope global \
  --output "${generated_dir}/authorization/invalid-response.json" \
  --expect-failure

python3 "${repo_root}/tests/e2e/home-issuer-client.py" issue \
  --port 19081 \
  --request "${generated_dir}/travel/enrollment-request.json" \
  --password-file "${generated_dir}/offline/test-password.txt" \
  --scope global \
  --output "${generated_dir}/authorization/enrollment-response.json"

docker run --rm \
  --user "${FLOWSPLICE_E2E_UID}:${FLOWSPLICE_E2E_GID}" \
  -e FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE=1 \
  -v "${generated_dir}/travel:/travel" \
  -v "${generated_dir}/authorization:/authorization:ro" \
  -v "${generated_dir}/certs:/certs:ro" \
  flowsplice-e2e:local \
  /usr/local/bin/flowsplice-travelagent enroll-import \
  --enrollment-dir /travel \
  --response /authorization/enrollment-response.json \
  --management-ca /certs/management-ca.crt \
  --business-ca /certs/business-ca.crt \
  --test-password-file /travel/test-password.txt

docker compose -f "${compose_file}" up -d travelagent
python3 "${repo_root}/tests/e2e/assert_e2e.py"
docker compose -f "${compose_file}" logs --no-color >"${log_file}" 2>&1
for event in \
  'event="relay_directory_updated"' \
  'event="carrier_race_started"' \
  'event="active_carrier_race_sent"' \
  'event="carrier_race_ack"' \
  'event="carrier_reevaluation_scheduled"' \
  'event="flow_detached"' \
  'event="carrier_recovery_started"' \
  'event="tcp_retransmit"'; do
  if ! grep -Fq "${event}" "${log_file}"; then
    echo "missing required E2E log event: ${event}" >&2
    exit 1
  fi
done
for event in \
  'event="travel_credential_revoked_by_home"' \
  'event="travel_authorization_published"' \
  'event="travel_authorization_ack"' \
  'event="travel_authorization_applied"' \
  'event="revoked_flow_closed"'; do
  if ! grep -Fq "${event}" "${log_file}"; then
    echo "missing required live-revocation E2E log event: ${event}" >&2
    exit 1
  fi
done
if ! grep -Eq 'event="carrier_reevaluation_scheduled".*stable=true.*switched=false' "${log_file}"; then
  echo 'missing stable active-Carrier reevaluation result' >&2
  exit 1
fi
docker compose -f "${compose_file}" ps
