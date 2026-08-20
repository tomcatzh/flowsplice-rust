#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
compose_file="${repo_root}/tests/e2e/compose.yaml"
generated_dir="${repo_root}/tests/e2e/generated"
log_file="${generated_dir}/e2e.log"
export FLOWSPLICE_E2E_UID="$(id -u)"
export FLOWSPLICE_E2E_GID="$(id -g)"
docker_pull="${FLOWSPLICE_DOCKER_PULL:-false}"
if [[ "${docker_pull}" != "false" && "${docker_pull}" != "true" ]]; then
  printf 'FLOWSPLICE_DOCKER_PULL must be true or false.\n' >&2
  exit 1
fi

mkdir -p "${generated_dir}"

if [[ "${FLOWSPLICE_E2E_REUSE_GENERATED:-0}" != "1" ]]; then
  "${repo_root}/tests/e2e/generate-certs.sh" prepare
else
  find "${generated_dir}/state" -maxdepth 1 -type f ! -name relay-pins.json -delete
  rm -rf "${generated_dir}/state/travel-enrollment"
  rm -rf "${generated_dir}/first-travel"
  rm -f \
    "${generated_dir}/offline/.flowsplice-issued-enrollments.json" \
    "${generated_dir}/offline-home2/.flowsplice-issued-enrollments.json"
  printf '%s\n' \
    '{"version":1,"snapshot":{"generation":1,"credentials":[],"revocations":[]},"used_enrollment_requests":[]}' \
    >"${generated_dir}/state/server-authorization.json"
  printf '{"next_generation":1}\n' \
    >"${generated_dir}/state/server-control-generation.json"
fi
# Re-render configuration templates even when the expensive certificate fixture is reused. This
# keeps newly added services and fault scenarios in sync with the checked-in templates.
server_pin="$(openssl x509 -in "${generated_dir}/certs/server.crt" -pubkey -noout \
  | openssl pkey -pubin -outform DER 2>/dev/null \
  | openssl dgst -sha256 \
  | sed 's/^.*= //')"
for template in "${repo_root}"/tests/e2e/config/*.toml; do
  output="${generated_dir}/config/$(basename -- "${template}")"
  sed -e "s/__SERVER_PIN__/${server_pin}/g" "${template}" >"${output}"
done
deployment_root_public_key="$(tr -d '\r\n' <"${generated_dir}/certs/deployment-root.pub")"
management_ca_certificate="$(cat "${generated_dir}/certs/management-ca.crt")"
docker build \
  --pull="${docker_pull}" \
  --build-arg "FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY=${deployment_root_public_key}" \
  --build-arg "FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM=${management_ca_certificate}" \
  --build-arg "FLOWSPLICE_BOOTSTRAP_RELAYS=relay1:8443,relay2:8443" \
  -f "${repo_root}/docker/e2e.Dockerfile" \
  -t flowsplice-e2e:local \
  "${repo_root}"
"${repo_root}/tests/e2e/generate-certs.sh" enroll-only

teardown() {
  docker compose -f "${compose_file}" down --volumes --remove-orphans >/dev/null 2>&1 || true
}

first_enroll_pid=""
finish() {
  status=$?
  set +e
  if [[ -n "${first_enroll_pid}" ]]; then
    kill "${first_enroll_pid}" >/dev/null 2>&1 || true
    wait "${first_enroll_pid}" >/dev/null 2>&1 || true
  fi
  docker compose -f "${compose_file}" logs --no-color >"${log_file}" 2>&1
  if (( status != 0 )); then
    tail -n 120 "${log_file}" >&2
  fi
  teardown
  trap - EXIT
  exit "${status}"
}

teardown
trap finish EXIT
docker compose -f "${compose_file}" up -d echo echo2 relay1 relay2 server homeagent homeagent2

if docker compose -f "${compose_file}" exec -T homeagent \
  test -e /issuer/deployment-root.key; then
  echo 'deployment root private key was exposed to Home Agent' >&2
  exit 1
fi

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

mkdir -p "${generated_dir}/first-travel"
cp "${generated_dir}/offline/test-password.txt" \
  "${generated_dir}/first-travel/test-password.txt"
chmod 600 "${generated_dir}/first-travel/test-password.txt"
if [[ -e "${generated_dir}/first-travel/travelagent.toml" || \
      -e "${generated_dir}/first-travel/cert" ]]; then
  echo 'first remote Travel was not clean before bootstrap' >&2
  exit 1
fi
docker compose -f "${compose_file}" run --no-deps --rm firsttravel \
  /usr/local/bin/flowsplice-travelagent enroll-remote \
  --travel-id first-remote-e2e \
  --home-id home-1 \
  --install-dir /first-travel \
  --tcp tcp-echo=0.0.0.0:10080 \
  --test-allow-remote-listen \
  --test-admin-token flowsplice-e2e-first-remote-administrator-token \
  --test-password-file /first-travel/test-password.txt \
  --wait-timeout-secs 180 \
  >"${generated_dir}/first-travel/enroll.log" 2>&1 &
first_enroll_pid=$!
bootstrap_pending="$(python3 "${repo_root}/tests/e2e/home-issuer-client.py" pending \
  --port 19081 \
  --travel-id first-remote-e2e \
  --wait-secs 120)"
bootstrap_request_id="$(python3 -c \
  'import json,sys; print(json.loads(sys.argv[1])["request_id"])' \
  "${bootstrap_pending}")"
bootstrap_verification_code="$(python3 -c \
  'import json,sys; value=json.loads(sys.argv[1]); assert value["bootstrap"] is True; print(value["verification_code"])' \
  "${bootstrap_pending}")"
if ! grep -Fq "Home verification code: ${bootstrap_verification_code}" \
  "${generated_dir}/first-travel/enroll.log"; then
  echo 'Home and Travel bootstrap verification codes did not match' >&2
  exit 1
fi
python3 "${repo_root}/tests/e2e/home-issuer-client.py" approve \
  --port 19081 \
  --request-id "${bootstrap_request_id}" \
  --password-file "${generated_dir}/offline/wrong-password.txt" \
  --scope global \
  --valid-days 365 \
  --expect-failure
python3 "${repo_root}/tests/e2e/home-issuer-client.py" approve \
  --port 19081 \
  --request-id "${bootstrap_request_id}" \
  --password-file "${generated_dir}/offline/test-password.txt" \
  --scope global \
  --valid-days 365 \
  --output "${generated_dir}/first-travel/approval.json"
wait "${first_enroll_pid}"
first_enroll_pid=""
for required in \
  travelagent.toml \
  cert/travel-management.crt \
  cert/travel-management.key \
  cert/travel-business.crt \
  cert/travel-business.key \
  state/travel-state.redb; do
  if [[ ! -s "${generated_dir}/first-travel/${required}" ]]; then
    echo "first remote Travel did not create ${required}" >&2
    exit 1
  fi
done
if [[ -e "${generated_dir}/first-travel/bootstrap-enrollment.json" ]]; then
  echo 'completed first enrollment retained its retrieval token' >&2
  exit 1
fi
if grep -Fq 'flowsplice-e2e-private-key-password' \
  "${generated_dir}/first-travel/travelagent.toml"; then
  echo 'generated Travel config contains a private-key password' >&2
  exit 1
fi
bootstrap_credential_id="$(python3 -c \
  'import json,sys; print(json.load(open(sys.argv[1]))["enrollment"]["approval"]["credential_id"])' \
  "${generated_dir}/first-travel/approval.json")"
docker compose -f "${compose_file}" up -d firsttravel
python3 "${repo_root}/tests/e2e/tcp-probe.py" \
  --port 12080 \
  --payload first-remote-enrollment-business \
  --wait-secs 90
bootstrap_acknowledged=0
for _ in $(seq 1 45); do
  current_pending="$(python3 "${repo_root}/tests/e2e/home-issuer-client.py" pending \
    --port 19081)"
  if python3 -c \
    'import json,sys; request_id=sys.argv[2]; assert all(item["request_id"] != request_id for item in json.loads(sys.argv[1]))' \
    "${current_pending}" "${bootstrap_request_id}"; then
    bootstrap_acknowledged=1
    break
  fi
  sleep 1
done
if (( bootstrap_acknowledged == 0 )); then
  echo 'first remote enrollment was not acknowledged and retired by Home' >&2
  exit 1
fi
first_travel_outbox="$(docker compose -f "${compose_file}" exec -T firsttravel \
  wget -qO- http://127.0.0.1:9080/api/enrollment)"
python3 -c \
  'import json,sys; request_id=sys.argv[2]; assert all(item["request_id"] != request_id for item in json.loads(sys.argv[1]))' \
  "${first_travel_outbox}" "${bootstrap_request_id}"
python3 "${repo_root}/tests/e2e/home-issuer-client.py" revoke \
  --port 19081 \
  --credential-id "${bootstrap_credential_id}" \
  --password-file "${generated_dir}/offline/test-password.txt" \
  --reason 'first remote enrollment E2E completed'
python3 "${repo_root}/tests/e2e/tcp-probe.py" \
  --port 12080 \
  --payload revoked-first-remote-enrollment \
  --wait-secs 90 \
  --expect-unavailable
docker compose -f "${compose_file}" stop firsttravel

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

python3 "${repo_root}/tests/e2e/tamper-enrollment-response.py" \
  --input "${generated_dir}/authorization/enrollment-response.json" \
  --output "${generated_dir}/authorization/tampered-enrollment-response.json" \
  --mode root-signature
python3 "${repo_root}/tests/e2e/tamper-enrollment-response.py" \
  --input "${generated_dir}/authorization/enrollment-response.json" \
  --output "${generated_dir}/authorization/spliced-certificate-response.json" \
  --mode certificate
python3 "${repo_root}/tests/e2e/tamper-enrollment-response.py" \
  --input "${generated_dir}/authorization/enrollment-response.json" \
  --output "${generated_dir}/authorization/spliced-request-response.json" \
  --mode request
for invalid_response in \
  tampered-enrollment-response.json \
  spliced-certificate-response.json \
  spliced-request-response.json; do
  if docker run --rm \
    --user "${FLOWSPLICE_E2E_UID}:${FLOWSPLICE_E2E_GID}" \
    -e FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE=1 \
    -v "${generated_dir}/travel:/travel" \
    -v "${generated_dir}/authorization:/authorization:ro" \
    flowsplice-e2e:local \
    /usr/local/bin/flowsplice-travelagent enroll-import \
    --enrollment-dir /travel \
    --response "/authorization/${invalid_response}" \
    --test-password-file /travel/test-password.txt; then
    echo "Travel accepted tampered Enrollment Response ${invalid_response}" >&2
    exit 1
  fi
done

docker run --rm \
  --user "${FLOWSPLICE_E2E_UID}:${FLOWSPLICE_E2E_GID}" \
  -e FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE=1 \
  -v "${generated_dir}/travel:/travel" \
  -v "${generated_dir}/authorization:/authorization:ro" \
  flowsplice-e2e:local \
  /usr/local/bin/flowsplice-travelagent enroll-import \
  --enrollment-dir /travel \
  --response /authorization/enrollment-response.json \
  --test-password-file /travel/test-password.txt

docker compose -f "${compose_file}" up -d travelagent
python3 -u "${repo_root}/tests/e2e/assert_e2e.py"
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
