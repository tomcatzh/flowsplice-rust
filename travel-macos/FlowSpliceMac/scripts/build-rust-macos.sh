#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd -- "${project_root}/../.." && pwd)"
rust_target="aarch64-apple-darwin"

if [[ "${ARCHS:-arm64}" != *arm64* ]]; then
  printf 'FlowSplice for macOS 26 supports Apple silicon only (ARCHS=%s).\n' "${ARCHS:-unset}" >&2
  exit 1
fi

cargo_args=(build --locked --package flowsplice-travel-apple --target "${rust_target}")
if [[ "${CONFIGURATION:-Debug}" == "Release" ]]; then
  cargo_args+=(--release)
fi

cd "${repo_root}"
cargo "${cargo_args[@]}"
