#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist"
deployment_root_public_key_file="${FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY_FILE:-${repo_root}/cert/deployment-root.pub}"
management_ca_certificate_file="${FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_FILE:-${repo_root}/cert/management-ca.crt}"
bootstrap_relays="${FLOWSPLICE_BOOTSTRAP_RELAYS:-}"
server_id="${FLOWSPLICE_SERVER_ID:-server-1}"
server_name="${FLOWSPLICE_SERVER_NAME:-server.flowsplice}"
server_control_port="${FLOWSPLICE_SERVER_CONTROL_PORT:-7443}"
home_ui_port="${FLOWSPLICE_HOME_UI_PORT:-9082}"
docker_pull="${FLOWSPLICE_DOCKER_PULL:-false}"
if [[ "${docker_pull}" != "false" && "${docker_pull}" != "true" ]]; then
  printf 'FLOWSPLICE_DOCKER_PULL must be true or false.\n' >&2
  exit 1
fi
if [[ ! -f "${deployment_root_public_key_file}" ]]; then
  printf 'Missing deployment root public key: %s\n' "${deployment_root_public_key_file}" >&2
  printf 'Set FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY_FILE to the public-key file for this deployment.\n' >&2
  exit 1
fi
if [[ ! -f "${management_ca_certificate_file}" ]]; then
  printf 'Missing bootstrap management CA certificate: %s\n' "${management_ca_certificate_file}" >&2
  printf 'Set FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_FILE to the public management CA certificate.\n' >&2
  exit 1
fi
if [[ -z "${bootstrap_relays}" ]]; then
  printf 'FLOWSPLICE_BOOTSTRAP_RELAYS must contain comma-separated public Relay management addresses.\n' >&2
  exit 1
fi
if [[ -z "${server_id}" || -z "${server_name}" \
      || ! "${server_control_port}" =~ ^[0-9]+$ \
      || "${server_control_port}" -lt 1 || "${server_control_port}" -gt 65535 \
      || ! "${home_ui_port}" =~ ^[0-9]+$ || "${home_ui_port}" -lt 1 \
      || "${home_ui_port}" -gt 65535 ]]; then
  printf 'Embedded Server id/name/control port or Home UI port is invalid.\n' >&2
  exit 1
fi
deployment_root_public_key="$(tr -d '\r\n' <"${deployment_root_public_key_file}")"
management_ca_certificate="$(cat "${management_ca_certificate_file}")"
if [[ ! "${deployment_root_public_key}" =~ ^04[0-9a-fA-F]{128}$ ]]; then
  printf 'Deployment root public key must be one uncompressed P-256 point in hexadecimal.\n' >&2
  exit 1
fi
mkdir -p "${dist_dir}/macos-arm64"

(cd "${repo_root}/travelagent/web" && npm ci && npm run build)
(cd "${repo_root}/homeagent/web" && npm ci && npm run build)
(cd "${repo_root}" && FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY="${deployment_root_public_key}" \
  FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM="${management_ca_certificate}" \
  FLOWSPLICE_BOOTSTRAP_RELAYS="${bootstrap_relays}" \
  FLOWSPLICE_SERVER_ID="${server_id}" \
  FLOWSPLICE_SERVER_NAME="${server_name}" \
  FLOWSPLICE_SERVER_CONTROL_PORT="${server_control_port}" \
  FLOWSPLICE_HOME_UI_PORT="${home_ui_port}" \
  cargo build --locked --release \
  -p flowsplice-server -p flowsplice-relay -p flowsplice-homeagent -p flowsplice-travelagent \
  -p flowsplice-foobar)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-enrollment --bin flowsplice-trust)
for binary in flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar flowsplice-trust; do
  cp "${repo_root}/target/release/${binary}" "${dist_dir}/macos-arm64/${binary}"
done

if ! command -v codesign >/dev/null 2>&1; then
  printf 'codesign is required to produce the macOS release artifacts.\n' >&2
  exit 1
fi
for binary in flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar flowsplice-trust; do
  identifier="io.zxf.flowsplice.${binary#flowsplice-}"
  artifact="${dist_dir}/macos-arm64/${binary}"
  codesign \
    --force \
    --sign - \
    --identifier "${identifier}" \
    --options runtime \
    --timestamp=none \
    "${artifact}"
  codesign --verify --strict --verbose=2 "${artifact}"
done

for spec in "amd64:x86_64-unknown-linux-musl" "arm64:aarch64-unknown-linux-musl"; do
  arch="${spec%%:*}"
  target="${spec#*:}"
  mkdir -p "${dist_dir}/linux-${arch}"
  docker buildx build \
    --pull="${docker_pull}" \
    --platform "linux/${arch}" \
    --build-arg "RUST_TARGET=${target}" \
    --build-arg "FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY=${deployment_root_public_key}" \
    --build-arg "FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM=${management_ca_certificate}" \
    --build-arg "FLOWSPLICE_BOOTSTRAP_RELAYS=${bootstrap_relays}" \
    --build-arg "FLOWSPLICE_SERVER_ID=${server_id}" \
    --build-arg "FLOWSPLICE_SERVER_NAME=${server_name}" \
    --build-arg "FLOWSPLICE_SERVER_CONTROL_PORT=${server_control_port}" \
    --build-arg "FLOWSPLICE_HOME_UI_PORT=${home_ui_port}" \
    --file "${repo_root}/docker/release.Dockerfile" \
    --output "type=local,dest=${dist_dir}/linux-${arch}" \
    "${repo_root}"
done

printf 'Release artifacts written below %s\n' "${dist_dir}"
printf 'macOS artifacts use ad-hoc signatures only; they are not Developer ID signed or notarized.\n'
