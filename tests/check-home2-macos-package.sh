#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
quick_start="${repo_root}/docs/HOME2_QUICK_START.zh-CN.md"
package_script="${repo_root}/scripts/build-home2-macos-package.sh"

for path in "${quick_start}" "${package_script}"; do
  [[ -f "${path}" ]] || { printf 'Missing Home 2 package input: %s\n' "${path}" >&2; exit 1; }
done

grep -q 'init --server <SERVER_IP>' "${quick_start}"
grep -q 'Serving-only' "${quick_start}"
grep -q 'Home issuer' "${quick_start}"
grep -q 'Global issuer' "${quick_start}"
grep -q 'Library/Application Support/FlowSplice/Home' "${quick_start}"
grep -q 'io.zxf.flowsplice.homeagent' "${package_script}"
grep -q 'FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY=' "${package_script}"
grep -q 'FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM=' "${package_script}"
grep -q 'FLOWSPLICE_SERVER_NAME=' "${package_script}"

if [[ $# -eq 0 ]]; then
  printf 'Home 2 macOS package inputs are consistent.\n'
  exit 0
fi
if [[ $# -ne 1 || ! -f "$1" ]]; then
  printf 'Usage: %s [package.tar.gz]\n' "$0" >&2
  exit 1
fi
if ! command -v codesign >/dev/null 2>&1; then
  printf 'codesign is required to verify the macOS package.\n' >&2
  exit 1
fi

archive="$1"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-home2-package-test.XXXXXX")"
cleanup() {
  rm -rf -- "${tmp_dir}"
}
trap cleanup EXIT

COPYFILE_DISABLE=1 tar -xzf "${archive}" -C "${tmp_dir}"
package_root="$(find "${tmp_dir}" -mindepth 1 -maxdepth 1 -type d -name 'flowsplice-home2-*-macos-arm64' -print -quit)"
[[ -n "${package_root}" ]] || { printf 'Package root is missing.\n' >&2; exit 1; }

for relative in \
  bin/flowsplice-homeagent \
  QUICK_START.zh-CN.md \
  SHA256SUMS; do
  [[ -f "${package_root}/${relative}" ]] || {
    printf 'Package member is missing: %s\n' "${relative}" >&2
    exit 1
  }
done

if [[ -e "${package_root}/config" || -e "${package_root}/launchd" ]]; then
  printf 'One-command Home package must not ship manual TOML or launchd templates.\n' >&2
  exit 1
fi

if find "${package_root}" -type f \( -name '*.key' -o -name '*.crt' -o -name '*.pem' \) -print -quit | grep -q .; then
  printf 'Home 2 package must not contain certificates or private keys.\n' >&2
  exit 1
fi
file "${package_root}/bin/flowsplice-homeagent" | grep -q 'Mach-O 64-bit executable arm64'
codesign --verify --strict --verbose=2 "${package_root}/bin/flowsplice-homeagent"
codesign_details="$(codesign -dvvv "${package_root}/bin/flowsplice-homeagent" 2>&1)"
if [[ "${codesign_details}" != *$'Identifier=io.zxf.flowsplice.homeagent\n'* \
  && "${codesign_details}" != *'Identifier=io.zxf.flowsplice.homeagent' ]]; then
  printf 'Home 2 binary has an unexpected codesign identifier.\n' >&2
  exit 1
fi
(
  cd "${package_root}"
  shasum -a 256 -c SHA256SUMS
)
printf 'Home 2 macOS package verified: %s\n' "${archive}"
