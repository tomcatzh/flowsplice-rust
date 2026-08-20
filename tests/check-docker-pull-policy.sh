#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

for script in "${repo_root}/scripts/build-release.sh" "${repo_root}/tests/e2e/run.sh"; do
  grep -Fq 'docker_pull="${FLOWSPLICE_DOCKER_PULL:-false}"' "${script}"
  grep -Fq -- '--pull="${docker_pull}"' "${script}"
  grep -Fq 'FLOWSPLICE_DOCKER_PULL must be true or false.' "${script}"
done
