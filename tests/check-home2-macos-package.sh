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
grep -q 'FLOWSPLICE_HOME_BOOTSTRAP_CONFIG_FILE' "${package_script}"
if grep -Eq 'FLOWSPLICE_(DEPLOYMENT_ROOT_PUBLIC_KEY|MANAGEMENT_CA_CERTIFICATE_PEM|SERVER_ID|SERVER_NAME|SERVER_CONTROL_PORT|HOME_UI_PORT)=' "${package_script}"; then
  printf 'Home package script must not compile deployment configuration into the binary.\n' >&2
  exit 1
fi

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
  home-bootstrap.toml \
  deployment-root.pub \
  deployment-trust.json \
  SHA256SUMS; do
  [[ -f "${package_root}/${relative}" ]] || {
    printf 'Package member is missing: %s\n' "${relative}" >&2
    exit 1
  }
done

if [[ -e "${package_root}/launchd" ]]; then
  printf 'One-command Home package must not ship a manual launchd template.\n' >&2
  exit 1
fi

if find "${package_root}" -type f \( -name '*.key' -o -name '*.crt' -o -name '*.pem' \) -print -quit | grep -q .; then
  printf 'Home 2 package must not contain certificates or private keys.\n' >&2
  exit 1
fi
file "${package_root}/bin/flowsplice-homeagent" | grep -q 'Mach-O 64-bit executable arm64'
grep -Fxq 'deployment_root_public_key = "deployment-root.pub"' \
  "${package_root}/home-bootstrap.toml"
grep -Fxq 'deployment_trust = "deployment-trust.json"' \
  "${package_root}/home-bootstrap.toml"
root_public_key="$(tr -d '\r\n' <"${package_root}/deployment-root.pub")"
if strings "${package_root}/bin/flowsplice-homeagent" | grep -Fq "${root_public_key}"; then
  printf 'Home binary contains the configured deployment root.\n' >&2
  exit 1
fi
trust_payload="${tmp_dir}/deployment-trust-payload.json"
jq -r '.payload_hex' "${package_root}/deployment-trust.json" | xxd -r -p >"${trust_payload}"
management_ca_body_line="$(jq -r '.management_ca_certificate_pem' "${trust_payload}" | sed -n '2p')"
for configured in \
  "$(jq -r '.deployment_id' "${trust_payload}")" \
  "${management_ca_body_line}"; do
  if strings "${package_root}/bin/flowsplice-homeagent" | grep -Fq "${configured}"; then
    printf 'Home binary contains deployment trust material.\n' >&2
    exit 1
  fi
done
for value in server_id server_name; do
  configured="$(awk -F'"' -v field="${value}" '$1 ~ "^" field " = " { print $2; exit }' "${package_root}/home-bootstrap.toml")"
  if [[ -n "${configured}" ]] \
    && strings "${package_root}/bin/flowsplice-homeagent" | grep -Fq "${configured}"; then
    printf 'Home binary contains configured %s.\n' "${value}" >&2
    exit 1
  fi
done
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
