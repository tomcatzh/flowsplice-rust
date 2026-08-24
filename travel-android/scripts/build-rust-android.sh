#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
output_root="${1:?Rust JNI output directory is required}"
android_sdk_root="${ANDROID_SDK_ROOT:-/Users/tomcat/Library/Android/sdk}"
android_ndk_root="${ANDROID_NDK_HOME:-${android_sdk_root}/ndk/29.0.14206865}"
toolchain_bin="${android_ndk_root}/toolchains/llvm/prebuilt/darwin-x86_64/bin"

if [[ ! -x "${toolchain_bin}/aarch64-linux-android34-clang" ]]; then
  printf 'Android NDK r29 is missing from %s\n' "${android_ndk_root}" >&2
  exit 1
fi

if [[ ! -d "${repo_root}/travelagent/web/dist" ]]; then
  npm ci --prefix "${repo_root}/travelagent/web"
  npm run build --prefix "${repo_root}/travelagent/web"
fi

build_abi() {
  local rust_target="$1"
  local android_abi="$2"
  local compiler_prefix="$3"
  local cargo_linker_name="CARGO_TARGET_$(printf '%s' "${rust_target}" | tr '[:lower:]-' '[:upper:]_')_LINKER"
  local cc_name="CC_$(printf '%s' "${rust_target}" | tr '-' '_')"
  local ar_name="AR_$(printf '%s' "${rust_target}" | tr '-' '_')"
  local compiler="${toolchain_bin}/${compiler_prefix}34-clang"

  env \
    ANDROID_NDK_HOME="${android_ndk_root}" \
    "${cargo_linker_name}=${compiler}" \
    "${cc_name}=${compiler}" \
    "${ar_name}=${toolchain_bin}/llvm-ar" \
    cargo build \
      --locked \
      --profile android-release \
      --target "${rust_target}" \
      -p flowsplice-travel-android

  mkdir -p "${output_root}/${android_abi}"
  cp \
    "${repo_root}/target/${rust_target}/android-release/libflowsplice_travel_android.so" \
    "${output_root}/${android_abi}/libflowsplice_travel_android.so"
}

build_abi aarch64-linux-android arm64-v8a aarch64-linux-android
build_abi x86_64-linux-android x86_64 x86_64-linux-android
