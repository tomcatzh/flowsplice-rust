#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist/macos-arm64"
version="$(awk -F'"' '/^version = "/ { print $2; exit }' "${repo_root}/Cargo.toml")"
package_name="flowsplice-home2-${version}-macos-arm64"
archive="${dist_dir}/${package_name}.tar.gz"
bootstrap_config_file="${FLOWSPLICE_HOME_BOOTSTRAP_CONFIG_FILE:-}"
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
if [[ -z "${bootstrap_config_file}" || ! -f "${bootstrap_config_file}" ]]; then
  printf 'FLOWSPLICE_HOME_BOOTSTRAP_CONFIG_FILE must name home-bootstrap.toml.\n' >&2
  exit 1
fi
bootstrap_dir="$(cd -- "$(dirname -- "${bootstrap_config_file}")" && pwd)"
deployment_root_public_key_file="${bootstrap_dir}/deployment-root.pub"
deployment_trust_file="${bootstrap_dir}/deployment-trust.json"
if [[ ! -f "${deployment_root_public_key_file}" || ! -f "${deployment_trust_file}" ]]; then
  printf 'Home bootstrap directory must contain deployment-root.pub and deployment-trust.json.\n' >&2
  exit 1
fi
if ! grep -Fxq 'deployment_root_public_key = "deployment-root.pub"' "${bootstrap_config_file}" \
  || ! grep -Fxq 'deployment_trust = "deployment-trust.json"' "${bootstrap_config_file}"; then
  printf 'Packaged Home bootstrap config must use adjacent deployment-root.pub and deployment-trust.json.\n' >&2
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
(cd "${repo_root}" && cargo build --locked --release -p flowsplice-homeagent)
bash "${repo_root}/tests/check-release-feature-gates.sh" \
  --home "${repo_root}/target/release/flowsplice-homeagent"

mkdir -p \
  "${package_root}/bin" \
  "${dist_dir}"
cp "${repo_root}/target/release/flowsplice-homeagent" \
  "${package_root}/bin/flowsplice-homeagent"
cp "${repo_root}/docs/HOME2_QUICK_START.zh-CN.md" \
  "${package_root}/QUICK_START.zh-CN.md"
cp "${bootstrap_config_file}" "${package_root}/home-bootstrap.toml"
cp "${deployment_root_public_key_file}" "${package_root}/deployment-root.pub"
cp "${deployment_trust_file}" "${package_root}/deployment-trust.json"
chmod 755 "${package_root}/bin/flowsplice-homeagent"

codesign \
  --force \
  --sign - \
  --identifier io.zxf.flowsplice.homeagent \
  --options runtime \
  --timestamp=none \
  "${package_root}/bin/flowsplice-homeagent"
codesign --verify --strict --verbose=2 "${package_root}/bin/flowsplice-homeagent"
(cd "${package_root}" && ./bin/flowsplice-homeagent check-bootstrap-config)

(
  cd "${package_root}"
  shasum -a 256 \
    bin/flowsplice-homeagent \
    QUICK_START.zh-CN.md \
    home-bootstrap.toml \
    deployment-root.pub \
    deployment-trust.json > SHA256SUMS
)

archive_tmp="${archive}.tmp"
COPYFILE_DISABLE=1 tar -czf "${archive_tmp}" -C "${work_dir}" "${package_name}"
mv -f "${archive_tmp}" "${archive}"
cp "${package_root}/bin/flowsplice-homeagent" "${dist_dir}/flowsplice-homeagent"

"${repo_root}/tests/check-home2-macos-package.sh" "${archive}"
printf 'Home 2 macOS package: %s\n' "${archive}"
shasum -a 256 "${archive}"
printf 'The binary is ad-hoc signed only; it is not Developer ID signed or notarized.\n'
