#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

if rg -n '\b(option_env|env)!\(' \
  travelagent homeagent server relay crates \
  --glob '*.rs'; then
  printf 'Production Rust source contains compile-time environment access.\n' >&2
  exit 1
fi

if rg -n 'FLOWSPLICE_(DEPLOYMENT_ROOT_PUBLIC_KEY|MANAGEMENT_CA_CERTIFICATE_PEM|BOOTSTRAP_RELAYS|SERVER_ID|SERVER_NAME|SERVER_CONTROL_PORT|HOME_UI_PORT)' \
  docker scripts tests/e2e/run.sh; then
  printf 'Build or E2E source still injects deployment configuration into compilation.\n' >&2
  exit 1
fi

for path in \
  homeagent/bootstrap.example.toml \
  travelagent/bootstrap.example.toml \
  tests/e2e/config/home-bootstrap.toml \
  tests/e2e/config/travel-bootstrap.toml; do
  [[ -s "${path}" ]] || { printf 'Missing bootstrap configuration: %s\n' "${path}" >&2; exit 1; }
done

grep -q '^deployment_root_public_key = ' travelagent/config.example.toml
grep -q '^deployment_root_public_key = ' tests/e2e/config/travelagent.toml
grep -q '^deployment_trust = ' travelagent/config.example.toml
grep -q '^deployment_trust = ' tests/e2e/config/travelagent.toml
printf 'Runtime deployment configuration boundary checks passed.\n'
