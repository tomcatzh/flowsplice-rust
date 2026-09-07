#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
compose_file="${repo_root}/tests/e2e/compose.yaml"
generated_dir="${repo_root}/tests/e2e/generated"
new_image="${FLOWSPLICE_E2E_IMAGE:?E2E image must be set}"
project="${COMPOSE_PROJECT_NAME:?E2E Compose project must be set}"
home_container="${project}-socket-home-$$"
home_created=0
home_stopped=0
travel_stopped=0

cleanup() {
  status=$?
  trap - EXIT
  set +e
  if (( home_created )); then
    docker logs "${home_container}" >"${generated_dir}/socket-home.log" 2>&1
    docker rm -f "${home_container}" >/dev/null 2>&1 || status=1
  fi
  if (( home_stopped )); then
    docker compose -f "${compose_file}" start homeagent2 >/dev/null || status=1
  fi
  if (( travel_stopped )); then
    docker compose -f "${compose_file}" start travelagent >/dev/null || status=1
  fi
  exit "${status}"
}
trap cleanup EXIT

# Add a service only in this disposable generated configuration. Existing certificates,
# authorization state, template and issuer files are retained untouched.
python3 - "${generated_dir}/config/homeagent2.toml" "${generated_dir}/config/socket-homeagent2.toml" <<'PY'
import pathlib
import sys
import tomllib
source, destination = map(pathlib.Path, sys.argv[1:])
text = source.read_text()
services = tomllib.loads(text).get("services", [])
matching = [service for service in services if service["id"] == "udp-echo"]
if matching and matching[0]["protocol"] != "udp":
    raise SystemExit("home-2 udp-echo has an unexpected protocol")
if not matching:
    text += '\n[[services]]\nid = "udp-echo"\nalias = "Socket UDP Echo"\nprotocol = "udp"\ntarget = "in-process"\n'
destination.write_text(text)
PY

probe_travel() {
  # Force the current image even during the optional old-Travel compatibility suite.
  FLOWSPLICE_E2E_TRAVEL_IMAGE="${new_image}" \
    docker compose -f "${compose_file}" run --rm --no-deps -T travelagent \
    /usr/local/bin/flowsplice-socket-probe travel \
    /config/travelagent.toml /travel/test-password.txt \
    /travel/cert/deployment-root.pub "$1"
}

travel_stopped=1
docker compose -f "${compose_file}" stop -t 10 travelagent >/dev/null
probe_travel home-1

home_stopped=1
docker compose -f "${compose_file}" stop -t 10 homeagent2 >/dev/null
docker create --name "${home_container}" \
  --network "${project}_flowsplice" \
  --user "${FLOWSPLICE_E2E_UID:-0}:${FLOWSPLICE_E2E_GID:-0}" \
  -e RUST_LOG=flowsplice_home_core=debug,info \
  -v "${generated_dir}/config:/config:ro" \
  -v "${generated_dir}/certs:/certs:ro" \
  -v "${generated_dir}/state:/state" \
  "${new_image}" /usr/local/bin/flowsplice-socket-probe home \
  /config/socket-homeagent2.toml >/dev/null
home_created=1
docker start "${home_container}" >/dev/null
# The Travel probe itself bounds encrypted discovery/connection and traffic at 120 seconds.
probe_travel home-2
docker stop -t 10 "${home_container}" >/dev/null
if [[ "$(docker inspect -f '{{.State.ExitCode}}' "${home_container}")" != "0" ]]; then
  docker logs "${home_container}" >&2
  printf 'Embedded Home failed graceful shutdown.\n' >&2
  exit 1
fi
docker logs "${home_container}" >"${generated_dir}/socket-home.log" 2>&1
python3 - "${generated_dir}/socket-home.log" <<'PY'
import pathlib
import sys
if '"checkpoint":"in-process-home-shutdown-store-reopened"' not in pathlib.Path(sys.argv[1]).read_text():
    raise SystemExit("Embedded Home did not release and reopen its persistent state")
PY
printf '%s\n' '{"checkpoint":"socket-api-existing-home-and-issuer-free-in-process-home"}'
