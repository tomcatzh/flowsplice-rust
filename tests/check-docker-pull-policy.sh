#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

for script in "${repo_root}/scripts/build-release.sh" "${repo_root}/tests/e2e/run.sh"; do
  grep -Fq 'docker_pull="${FLOWSPLICE_DOCKER_PULL:-false}"' "${script}"
  grep -Fq -- '--pull="${docker_pull}"' "${script}"
  grep -Fq 'FLOWSPLICE_DOCKER_PULL must be true or false.' "${script}"
done

for dockerfile in "${repo_root}/docker/e2e.Dockerfile" "${repo_root}/docker/release.Dockerfile"; do
  grep -Eq '^# syntax=docker/dockerfile:[^@]+@sha256:[0-9a-f]{64}$' "${dockerfile}"
  if grep '^FROM ' "${dockerfile}" | grep -v '^FROM scratch' | grep -vq '@sha256:'; then
    printf 'Every non-scratch base image must be digest-pinned: %s\n' "${dockerfile}" >&2
    exit 1
  fi
  grep -Fq 'ENV RUSTUP_TOOLCHAIN=1.97.1' "${dockerfile}"
done

grep -Fq 'channel = "1.97.1"' "${repo_root}/rust-toolchain.toml"

for variable in \
  FLOWSPLICE_SERVER_ID \
  FLOWSPLICE_SERVER_NAME \
  FLOWSPLICE_SERVER_CONTROL_PORT \
  FLOWSPLICE_HOME_UI_PORT; do
  grep -Fq "ARG ${variable}" "${repo_root}/docker/release.Dockerfile"
  grep -Fq -- "--build-arg \"${variable}=" "${repo_root}/scripts/build-release.sh"
done
