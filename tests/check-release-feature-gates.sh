#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

production_build_files=(
  docker/release.Dockerfile
  scripts/build-release.sh
  scripts/build-home2-macos-package.sh
  scripts/build-travel-macos-package.sh
)
if rg -n -- 'e2e-remote-ui|--all-features' "${production_build_files[@]}"; then
  printf 'A production build path enables an E2E-only Cargo feature.\n' >&2
  exit 1
fi

assert_rejects_e2e_config() {
  local label="$1"
  local binary="$2"
  local config="$3"
  local output
  local status
  set +e
  output="$("${binary}" --config "${config}" 2>&1)"
  status=$?
  set -e
  if (( status == 0 )); then
    printf '%s release artifact accepted E2E-only configuration.\n' "${label}" >&2
    exit 1
  fi
  if [[ "${output}" != *"unknown field \`test_allow_remote_listen\`"* \
    && "${output}" != *"unknown field \`test_admin_token\`"* ]]; then
    printf '%s release artifact did not prove that E2E-only fields are absent:\n%s\n' \
      "${label}" "${output}" >&2
    exit 1
  fi
}

while (( $# > 0 )); do
  case "$1" in
    --home)
      [[ $# -ge 2 ]] || { printf '%s requires a binary path.\n' "$1" >&2; exit 2; }
      assert_rejects_e2e_config \
        Home "$2" "${repo_root}/tests/e2e/config/homeagent.toml"
      shift 2
      ;;
    --travel)
      [[ $# -ge 2 ]] || { printf '%s requires a binary path.\n' "$1" >&2; exit 2; }
      assert_rejects_e2e_config \
        Travel "$2" "${repo_root}/tests/e2e/config/travelagent.toml"
      shift 2
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

printf 'Release E2E-feature exclusion checks passed.\n'
