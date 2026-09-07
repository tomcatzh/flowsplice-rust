#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd -- "${SRCROOT}/.." && pwd)"
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:${PATH}"
case "${PLATFORM_NAME}" in
  macosx) rust_target=aarch64-apple-darwin ;;
  iphonesimulator) rust_target=aarch64-apple-ios-sim ;;
  iphoneos) rust_target=aarch64-apple-ios ;;
  *) echo 'Unsupported PTY Apple platform' >&2; exit 1 ;;
esac
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${repo_root}/target}"
args=(build --locked -p flowsplice-pty-native --target "${rust_target}")
profile=debug
if [[ "${CONFIGURATION}" == Release ]]; then args+=(--release); profile=release; fi
python3 "${SRCROOT}/scripts/stage-resources.py"
cd "${repo_root}"
cargo "${args[@]}"
mkdir -p "${BUILT_PRODUCTS_DIR}/rust"
cp "${CARGO_TARGET_DIR}/${rust_target}/${profile}/libflowsplice_pty_native.a" "${BUILT_PRODUCTS_DIR}/rust/"
