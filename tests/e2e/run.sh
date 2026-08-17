#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
compose_file="${repo_root}/tests/e2e/compose.yaml"

"${repo_root}/tests/e2e/generate-certs.sh"
docker build -f "${repo_root}/docker/e2e.Dockerfile" -t flowsplice-e2e:local "${repo_root}"

cleanup() {
  docker compose -f "${compose_file}" down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup
docker compose -f "${compose_file}" up -d
python3 "${repo_root}/tests/e2e/assert_e2e.py"
cp \
  "${repo_root}/tests/e2e/generated/config/homeagent.updated.toml" \
  "${repo_root}/tests/e2e/generated/config/homeagent.toml"
docker compose -f "${compose_file}" restart homeagent
python3 "${repo_root}/tests/e2e/assert_e2e.py" catalog-update
catalog_connections="$(docker compose -f "${compose_file}" logs --no-color travelagent \
  | grep -c 'catalog subscription connected' || true)"
if [[ "${catalog_connections}" != "1" ]]; then
  printf 'Expected one long-lived catalog connection, observed %s\n' "${catalog_connections}" >&2
  exit 1
fi
docker compose -f "${compose_file}" ps
