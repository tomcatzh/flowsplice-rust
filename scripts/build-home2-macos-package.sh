#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist/macos-arm64"
version="$(awk -F'"' '/^version = "/ { print $2; exit }' "${repo_root}/Cargo.toml")"
package_name="flowsplice-home2-${version}-macos-arm64"
archive="${dist_dir}/${package_name}.tar.gz"
deployment_root_public_key_file="${FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY_FILE:-${repo_root}/cert/deployment-root.pub}"
management_ca_certificate_file="${FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_FILE:-${repo_root}/cert/management-ca.crt}"
server_id="${FLOWSPLICE_SERVER_ID:-server-1}"
server_name="${FLOWSPLICE_SERVER_NAME:-server.flowsplice}"
server_control_port="${FLOWSPLICE_SERVER_CONTROL_PORT:-7443}"
home_ui_port="${FLOWSPLICE_HOME_UI_PORT:-9082}"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-home2-package.XXXXXX")"
package_root="${work_dir}/${package_name}"

cleanup() {
  rm -rf -- "${work_dir}"
}
trap cleanup EXIT

if [[ -z "${version}" ]]; then
  printf 'Unable to read the workspace version from Cargo.toml.\n' >&2
  exit 1
fi
if [[ ! -f "${deployment_root_public_key_file}" ]]; then
  printf 'Missing deployment root public key: %s\n' "${deployment_root_public_key_file}" >&2
  exit 1
fi
if [[ ! -f "${management_ca_certificate_file}" ]]; then
  printf 'Missing bootstrap management CA certificate: %s\n' "${management_ca_certificate_file}" >&2
  exit 1
fi
deployment_root_public_key="$(tr -d '\r\n' <"${deployment_root_public_key_file}")"
management_ca_certificate="$(cat "${management_ca_certificate_file}")"
if [[ ! "${deployment_root_public_key}" =~ ^04[0-9a-fA-F]{128}$ ]]; then
  printf 'Deployment root public key must be one uncompressed P-256 point in hexadecimal.\n' >&2
  exit 1
fi
if [[ -z "${server_id}" || -z "${server_name}" \
      || ! "${server_control_port}" =~ ^[0-9]+$ \
      || "${server_control_port}" -lt 1 || "${server_control_port}" -gt 65535 \
      || ! "${home_ui_port}" =~ ^[0-9]+$ || "${home_ui_port}" -lt 1 \
      || "${home_ui_port}" -gt 65535 ]]; then
  printf 'Embedded Server id/control port or Home UI port is invalid.\n' >&2
  exit 1
fi
if [[ ! -d "${repo_root}/homeagent/web/node_modules" ]]; then
  printf 'Missing Home web dependencies. Run npm ci in homeagent/web once before packaging.\n' >&2
  exit 1
fi
if ! command -v codesign >/dev/null 2>&1; then
  printf 'codesign is required to produce the macOS package.\n' >&2
  exit 1
fi

(cd "${repo_root}/homeagent/web" && npm run build)
(cd "${repo_root}" && \
  FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY="${deployment_root_public_key}" \
  FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM="${management_ca_certificate}" \
  FLOWSPLICE_SERVER_ID="${server_id}" \
  FLOWSPLICE_SERVER_NAME="${server_name}" \
  FLOWSPLICE_SERVER_CONTROL_PORT="${server_control_port}" \
  FLOWSPLICE_HOME_UI_PORT="${home_ui_port}" \
  cargo build --locked --release -p flowsplice-homeagent)

mkdir -p \
  "${package_root}/bin" \
  "${dist_dir}"
cp "${repo_root}/target/release/flowsplice-homeagent" \
  "${package_root}/bin/flowsplice-homeagent"
cp "${repo_root}/docs/HOME2_QUICK_START.zh-CN.md" \
  "${package_root}/QUICK_START.zh-CN.md"
chmod 755 "${package_root}/bin/flowsplice-homeagent"

codesign \
  --force \
  --sign - \
  --identifier io.zxf.flowsplice.homeagent \
  --options runtime \
  --timestamp=none \
  "${package_root}/bin/flowsplice-homeagent"
codesign --verify --strict --verbose=2 "${package_root}/bin/flowsplice-homeagent"

(
  cd "${package_root}"
  shasum -a 256 \
    bin/flowsplice-homeagent \
    QUICK_START.zh-CN.md > SHA256SUMS
)

archive_tmp="${archive}.tmp"
COPYFILE_DISABLE=1 tar -czf "${archive_tmp}" -C "${work_dir}" "${package_name}"
mv -f "${archive_tmp}" "${archive}"
cp "${package_root}/bin/flowsplice-homeagent" "${dist_dir}/flowsplice-homeagent"

"${repo_root}/tests/check-home2-macos-package.sh" "${archive}"
printf 'Home 2 macOS package: %s\n' "${archive}"
shasum -a 256 "${archive}"
printf 'The binary is ad-hoc signed only; it is not Developer ID signed or notarized.\n'
