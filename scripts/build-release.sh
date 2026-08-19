#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist"
deployment_root_public_key_file="${FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY_FILE:-${repo_root}/cert/deployment-root.pub}"
if [[ ! -f "${deployment_root_public_key_file}" ]]; then
  printf 'Missing deployment root public key: %s\n' "${deployment_root_public_key_file}" >&2
  printf 'Set FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY_FILE to the public-key file for this deployment.\n' >&2
  exit 1
fi
deployment_root_public_key="$(tr -d '\r\n' <"${deployment_root_public_key_file}")"
if [[ ! "${deployment_root_public_key}" =~ ^04[0-9a-fA-F]{128}$ ]]; then
  printf 'Deployment root public key must be one uncompressed P-256 point in hexadecimal.\n' >&2
  exit 1
fi
mkdir -p "${dist_dir}/macos-arm64"

(cd "${repo_root}/travelagent/web" && npm ci && npm run build)
(cd "${repo_root}/homeagent/web" && npm ci && npm run build)
(cd "${repo_root}" && FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY="${deployment_root_public_key}" \
  cargo build --locked --release \
  -p flowsplice-server -p flowsplice-relay -p flowsplice-homeagent -p flowsplice-travelagent \
  -p flowsplice-foobar)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-enrollment --bin flowsplice-trust)
for binary in flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar flowsplice-trust; do
  cp "${repo_root}/target/release/${binary}" "${dist_dir}/macos-arm64/${binary}"
done

for spec in "amd64:x86_64-unknown-linux-musl" "arm64:aarch64-unknown-linux-musl"; do
  arch="${spec%%:*}"
  target="${spec#*:}"
  mkdir -p "${dist_dir}/linux-${arch}"
  docker buildx build \
    --platform "linux/${arch}" \
    --build-arg "RUST_TARGET=${target}" \
    --build-arg "FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY=${deployment_root_public_key}" \
    --file "${repo_root}/docker/release.Dockerfile" \
    --output "type=local,dest=${dist_dir}/linux-${arch}" \
    "${repo_root}"
done

printf 'Release artifacts written below %s\n' "${dist_dir}"
