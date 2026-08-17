#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
compose_file="${repo_root}/tests/e2e/compose.yaml"
generated_dir="${repo_root}/tests/e2e/generated"
log_file="${generated_dir}/e2e.log"

mkdir -p "${generated_dir}"

"${repo_root}/tests/e2e/generate-certs.sh"
docker build -f "${repo_root}/docker/e2e.Dockerfile" -t flowsplice-e2e:local "${repo_root}"

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
docker compose -f "${compose_file}" up -d
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
  'event="travel_credential_revoked"' \
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
