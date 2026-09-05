#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
helper="${repo_root}/scripts/private-travel-trust.py"
profile_file=''
profile_alias=''
ref='HEAD'
check_only=false

usage() {
  cat <<'USAGE'
Usage: build-private-travel-packages.sh (--profile NAME | --profile-file ABSOLUTE_PATH) [--ref REF] [--check-only]
USAGE
}

fail() {
  printf 'Private Travel packaging arguments are invalid.\n' >&2
  exit 2
}

while (($#)); do
  case "$1" in
    --profile)
      (($# >= 2)) || fail
      [[ -z "${profile_file}" && -z "${profile_alias}" ]] || fail
      profile_alias="$2"
      [[ "${profile_alias}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || fail
      shift 2
      ;;
    --profile-file)
      (($# >= 2)) || fail
      [[ -z "${profile_file}" && -z "${profile_alias}" ]] || fail
      profile_file="$2"
      [[ "${profile_file}" = /* ]] || fail
      shift 2
      ;;
    --ref)
      (($# >= 2)) || fail
      ref="$2"
      [[ -n "${ref}" ]] || fail
      shift 2
      ;;
    --check-only)
      check_only=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail
      ;;
  esac
done

if [[ -n "${profile_alias}" ]]; then
  [[ -n "${HOME:-}" ]] || fail
  profile_file="${HOME}/.config/flowsplice/private-packaging/profiles/${profile_alias}.toml"
fi
[[ -n "${profile_file}" ]] || fail

arguments=(build --profile-file "${profile_file}" --ref "${ref}")
if [[ "${check_only}" == 'true' ]]; then
  arguments+=(--check-only)
fi
exec python3 "${helper}" "${arguments[@]}"
