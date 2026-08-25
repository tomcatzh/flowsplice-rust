#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd -- "${project_root}/../.." && pwd)"

case "${PLATFORM_NAME:-iphonesimulator}" in
  iphonesimulator)
    rust_target="aarch64-apple-ios-sim"
    ;;
  iphoneos)
    rust_target="aarch64-apple-ios"
    ;;
  *)
    printf 'Unsupported Apple platform: %s\n' "${PLATFORM_NAME:-unset}" >&2
    exit 1
    ;;
esac

cargo_args=(build --package flowsplice-travel-apple --target "${rust_target}")
if [[ "${CONFIGURATION:-Debug}" == "Release" ]]; then
  cargo_args+=(--release)
fi

cd "${repo_root}"
cargo "${cargo_args[@]}"
