#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
serving_config="${repo_root}/homeagent/config.home2-serving.example.toml"
issuer_config="${repo_root}/homeagent/config.home2-issuer.example.toml"
quick_start="${repo_root}/docs/HOME2_QUICK_START.zh-CN.md"
package_script="${repo_root}/scripts/build-home2-macos-package.sh"

for path in "${serving_config}" "${issuer_config}" "${quick_start}" "${package_script}"; do
  [[ -f "${path}" ]] || { printf 'Missing Home 2 package input: %s\n' "${path}" >&2; exit 1; }
done

grep -q '^id = "home-2"$' "${serving_config}"
grep -q '^id = "home-2"$' "${issuer_config}"
grep -q '__HOME2_ROOT__/state/home2-state.redb' "${serving_config}"
grep -q '__HOME2_ROOT__/state/home2-state.redb' "${issuer_config}"
grep -q '^ui_listen = "127.0.0.1:9082"$' "${serving_config}"
grep -q '^ui_listen = "127.0.0.1:9082"$' "${issuer_config}"
if grep -q '^\[issuer\]' "${serving_config}"; then
  printf 'Serving-only Home 2 configuration unexpectedly enables issuer operations.\n' >&2
  exit 1
fi
grep -q '^\[issuer\]' "${issuer_config}"
grep -q '^\[issuer.home_authority\]' "${issuer_config}"
if grep -q '^\[issuer.global_authority\]' "${issuer_config}"; then
  printf 'Home 2 issuer example must not provision global authority by default.\n' >&2
  exit 1
fi
grep -q 'Serving-only' "${quick_start}"
grep -q 'Home-2 issuer' "${quick_start}"
grep -q 'deployment trust' "${quick_start}"
grep -q 'io.zxf.flowsplice.homeagent' "${package_script}"

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
  config/homeagent-serving-only.toml \
  config/homeagent-issuer.toml \
  launchd/io.zxf.flowsplice.home2.plist \
  QUICK_START.zh-CN.md \
  SHA256SUMS; do
  [[ -f "${package_root}/${relative}" ]] || {
    printf 'Package member is missing: %s\n' "${relative}" >&2
    exit 1
  }
done

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
plutil -lint "${package_root}/launchd/io.zxf.flowsplice.home2.plist" >/dev/null
printf 'Home 2 macOS package verified: %s\n' "${archive}"
