#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

for script in "${repo_root}/scripts/build-release.sh" "${repo_root}/tests/e2e/run.sh"; do
  grep -Fq 'docker_pull="${FLOWSPLICE_DOCKER_PULL:-false}"' "${script}"
  grep -Fq -- '--pull="${docker_pull}"' "${script}"
  grep -Fq 'FLOWSPLICE_DOCKER_PULL must be true or false.' "${script}"
  grep -Fq 'rust_mirror_url="${RUST_MIRROR_URL-http://host.docker.internal:18787}"' "${script}"
  grep -Fq '"${rust_mirror_url}" != "off"' "${script}"
  grep -Fq -- '--build-arg "RUST_MIRROR_URL=${rust_mirror_url}"' "${script}"
  grep -Fq -- '--build-arg "RUSTUP_DIST_SERVER=${rustup_dist_server}"' "${script}"
  grep -Fq -- '--build-arg "RUSTUP_UPDATE_ROOT=${rustup_update_root}"' "${script}"
done

for dockerfile in "${repo_root}/docker/e2e.Dockerfile" "${repo_root}/docker/release.Dockerfile"; do
  grep -Eq '^# syntax=docker/dockerfile:[^@]+@sha256:[0-9a-f]{64}$' "${dockerfile}"
  if grep '^FROM ' "${dockerfile}" | grep -v '^FROM scratch' | grep -vq '@sha256:'; then
    printf 'Every non-scratch base image must be digest-pinned: %s\n' "${dockerfile}" >&2
    exit 1
  fi
  grep -Fq 'ENV RUSTUP_TOOLCHAIN=1.97.1' "${dockerfile}"
  grep -Fq 'ARG TARGETARCH' "${dockerfile}"
  grep -Fq 'ARG RUST_MIRROR_URL' "${dockerfile}"
  grep -Fq 'ARG RUSTUP_DIST_SERVER' "${dockerfile}"
  grep -Fq 'ARG RUSTUP_UPDATE_ROOT' "${dockerfile}"
  grep -Fq 'source.crates-io.replace-with="flowsplice-mirror"' "${dockerfile}"
  grep -Fq 'source.flowsplice-mirror.registry=' "${dockerfile}"
  grep -Fq '${RUST_MIRROR_URL%/}/index/' "${dockerfile}"
  grep -Eq 'id=flowsplice-[^,]+-cargo-registry-\$\{TARGETARCH\}' "${dockerfile}"
  grep -Eq 'id=flowsplice-[^,]+-cargo-git-\$\{TARGETARCH\}' "${dockerfile}"
  grep -Eq 'id=flowsplice-[^,]+-cargo-target-\$\{TARGETARCH\}' "${dockerfile}"
done

grep -Fq 'channel = "1.97.1"' "${repo_root}/rust-toolchain.toml"

if rg -n 'FLOWSPLICE_(DEPLOYMENT_ROOT_PUBLIC_KEY|MANAGEMENT_CA_CERTIFICATE_PEM|BOOTSTRAP_RELAYS|SERVER_ID|SERVER_NAME|SERVER_CONTROL_PORT|HOME_UI_PORT)' \
  "${repo_root}/docker" "${repo_root}/scripts/build-release.sh" "${repo_root}/tests/e2e/run.sh"; then
  printf 'Release and E2E builds must be deployment-neutral.\n' >&2
  exit 1
fi
