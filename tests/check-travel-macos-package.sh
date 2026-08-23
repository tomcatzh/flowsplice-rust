#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
quick_start="${repo_root}/docs/QUICK_START.zh-CN.md"
package_script="${repo_root}/scripts/build-travel-macos-package.sh"

for path in "${quick_start}" "${package_script}"; do
  [[ -f "${path}" ]] || { printf 'Missing Travel package input: %s\n' "${path}" >&2; exit 1; }
done

grep -q 'travel-bootstrap.example.toml' "${quick_start}"
grep -q 'io.zxf.flowsplice.travelagent' "${package_script}"
grep -q 'travelagent/bootstrap.example.toml' "${package_script}"
grep -q 'check-package-privacy.py' "${package_script}"
if grep -q 'FLOWSPLICE_TRAVEL_BOOTSTRAP_CONFIG_FILE' "${package_script}"; then
  printf 'Public Travel packaging must not accept a deployment configuration input.\n' >&2
  exit 1
fi
if grep -Eq 'FLOWSPLICE_(DEPLOYMENT_ROOT_PUBLIC_KEY|MANAGEMENT_CA_CERTIFICATE_PEM|BOOTSTRAP_RELAYS)=' "${package_script}"; then
  printf 'Travel package script must not compile deployment configuration into the binary.\n' >&2
  exit 1
fi

if [[ $# -eq 0 ]]; then
  printf 'Travel macOS package inputs are consistent.\n'
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
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-travel-package-test.XXXXXX")"
cleanup() {
  rm -rf -- "${tmp_dir}"
}
trap cleanup EXIT

COPYFILE_DISABLE=1 tar -xzf "${archive}" -C "${tmp_dir}"
package_root="$(find "${tmp_dir}" -mindepth 1 -maxdepth 1 -type d -name 'flowsplice-travel-*-macos-arm64' -print -quit)"
[[ -n "${package_root}" ]] || { printf 'Package root is missing.\n' >&2; exit 1; }

for relative in \
  bin/flowsplice-travelagent \
  QUICK_START.zh-CN.md \
  travel-bootstrap.example.toml \
  SHA256SUMS; do
  [[ -f "${package_root}/${relative}" ]] || {
    printf 'Package member is missing: %s\n' "${relative}" >&2
    exit 1
  }
done
if [[ "$(find "${package_root}" -type f | wc -l | tr -d ' ')" -ne 4 ]]; then
  printf 'Travel public package contains files outside the allowlist.\n' >&2
  exit 1
fi
for forbidden in travel-bootstrap.toml deployment-root.pub deployment-trust.json; do
  if find "${package_root}" -type f -name "${forbidden}" -print -quit | grep -q .; then
    printf 'Travel public package contains deployment material.\n' >&2
    exit 1
  fi
done
if find "${package_root}" -type l -print -quit | grep -q .; then
  printf 'Travel public package must not contain symbolic links.\n' >&2
  exit 1
fi
if find "${package_root}" -type f \( -name '*.key' -o -name '*.crt' -o -name '*.pem' \) -print -quit | grep -q .; then
  printf 'Travel package must not contain private keys or standalone certificates.\n' >&2
  exit 1
fi
python3 "${repo_root}/scripts/check-package-privacy.py" \
  "${package_root}/travel-bootstrap.example.toml"
while IFS= read -r relay; do
  [[ -z "${relay}" ]] && continue
  if strings "${package_root}/bin/flowsplice-travelagent" | grep -Fq "${relay}"; then
    printf 'Travel binary contains a configured Relay value.\n' >&2
    exit 1
  fi
done < <(sed -n 's/^[[:space:]]*"\([^"]*\)"[,]\{0,1\}$/\1/p' "${package_root}/travel-bootstrap.example.toml")
file "${package_root}/bin/flowsplice-travelagent" | grep -q 'Mach-O 64-bit executable arm64'
codesign --verify --strict --verbose=2 "${package_root}/bin/flowsplice-travelagent"
codesign_details="$(codesign -dvvv "${package_root}/bin/flowsplice-travelagent" 2>&1)"
if [[ "${codesign_details}" != *'Identifier=io.zxf.flowsplice.travelagent'* ]]; then
  printf 'Travel binary has an unexpected codesign identifier.\n' >&2
  exit 1
fi
(
  cd "${package_root}"
  shasum -a 256 -c SHA256SUMS
)
printf 'Travel macOS package verified: %s\n' "${archive}"
