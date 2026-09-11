#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
rust_target="${1:?Choose aarch64-linux-android or x86_64-linux-android}"
case "$rust_target" in
  aarch64-linux-android) abi=arm64-v8a ;;
  x86_64-linux-android) abi=x86_64 ;;
  *) exit 2 ;;
esac
sdk="${ANDROID_SDK_ROOT:-${HOME}/Library/Android/sdk}"
ndk="${ANDROID_NDK_HOME:-${sdk}/ndk/29.0.14206865}"
toolchain="${ndk}/toolchains/llvm/prebuilt/darwin-x86_64/bin"
target_root="${CARGO_TARGET_DIR:?Set external CARGO_TARGET_DIR}"
python3 "$repo_root/scripts/private-travel-trust.py" check-path --path "$target_root"
linker_name="CARGO_TARGET_$(printf '%s' "$rust_target" | tr '[:lower:]-' '[:upper:]_')_LINKER"
cc_name="CC_$(printf '%s' "$rust_target" | tr '-' '_')"
cxx_name="CXX_$(printf '%s' "$rust_target" | tr '-' '_')"
ar_name="AR_$(printf '%s' "$rust_target" | tr '-' '_')"
# JNI must be self-contained apart from Android's declared system libraries.
# Shared-library linking otherwise permits missing C++ ABI symbols until dlopen.
env "${linker_name}=${toolchain}/${rust_target}34-clang" "${cc_name}=${toolchain}/${rust_target}34-clang" "${cxx_name}=${toolchain}/${rust_target}34-clang++" "${ar_name}=${toolchain}/llvm-ar" cargo rustc --locked --profile android-release --target "$rust_target" -p flowsplice-pty-native --lib --manifest-path "$repo_root/Cargo.toml" -- -C link-arg=-Wl,--no-undefined
mkdir -p "$repo_root/pty-android/app/src/main/jniLibs/$abi"
cp "$target_root/$rust_target/android-release/libflowsplice_pty_native.so" "$repo_root/pty-android/app/src/main/jniLibs/$abi/"
