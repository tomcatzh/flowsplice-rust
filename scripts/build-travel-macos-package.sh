#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${repo_root}/dist/macos-arm64"
version="$(awk -F'"' '/^version = "/ { print $2; exit }' "${repo_root}/Cargo.toml")"
package_name="flowsplice-travel-${version}-macos-arm64"
archive="${dist_dir}/${package_name}.tar.gz"
sample_config_file="${repo_root}/travelagent/bootstrap.example.toml"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-travel-package.XXXXXX")"
package_root="${work_dir}/${package_name}"

cleanup() {
  rm -rf -- "${work_dir}"
}
trap cleanup EXIT

if [[ -z "${version}" ]]; then
  printf 'Unable to read the workspace version from Cargo.toml.\n' >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  printf 'python3 is required to validate distributable package privacy.\n' >&2
  exit 1
fi
if [[ ! -f "${sample_config_file}" ]]; then
  printf 'Missing public Travel bootstrap sample.\n' >&2
  exit 1
fi
python3 "${repo_root}/scripts/check-package-privacy.py" "${sample_config_file}"
if [[ ! -d "${repo_root}/travelagent/web/node_modules" ]]; then
  printf 'Missing Travel web dependencies. Run npm ci in travelagent/web once before packaging.\n' >&2
  exit 1
fi
if ! command -v codesign >/dev/null 2>&1; then
  printf 'codesign is required to produce the macOS package.\n' >&2
  exit 1
fi

(cd "${repo_root}/travelagent/web" && npm run build)
(cd "${repo_root}" && cargo build --locked --release -p flowsplice-travelagent)
bash "${repo_root}/tests/check-release-feature-gates.sh" \
  --travel "${repo_root}/target/release/flowsplice-travelagent"

mkdir -p "${package_root}/bin" "${dist_dir}"
cp "${repo_root}/target/release/flowsplice-travelagent" \
  "${package_root}/bin/flowsplice-travelagent"
cp "${repo_root}/docs/QUICK_START.zh-CN.md" "${package_root}/QUICK_START.zh-CN.md"
cp "${sample_config_file}" "${package_root}/travel-bootstrap.example.toml"
chmod 755 "${package_root}/bin/flowsplice-travelagent"

codesign \
  --force \
  --sign - \
  --identifier io.zxf.flowsplice.travelagent \
  --options runtime \
  --timestamp=none \
  "${package_root}/bin/flowsplice-travelagent"
codesign --verify --strict --verbose=2 "${package_root}/bin/flowsplice-travelagent"
(
  cd "${package_root}"
  shasum -a 256 \
    bin/flowsplice-travelagent \
    QUICK_START.zh-CN.md \
    travel-bootstrap.example.toml > SHA256SUMS
)

archive_tmp="${archive}.tmp"
COPYFILE_DISABLE=1 tar -czf "${archive_tmp}" -C "${work_dir}" "${package_name}"
mv -f "${archive_tmp}" "${archive}"
cp "${package_root}/bin/flowsplice-travelagent" "${dist_dir}/flowsplice-travelagent"

"${repo_root}/tests/check-travel-macos-package.sh" "${archive}"
printf 'Travel macOS package: %s\n' "${archive}"
shasum -a 256 "${archive}"
printf 'The binary is ad-hoc signed only; it is not Developer ID signed or notarized.\n'
