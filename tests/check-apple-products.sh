#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
package_script="${repo_root}/scripts/build-apple-products.sh"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${repo_root}/Cargo.toml" | head -n 1)"
team_id="${FLOWSPLICE_APPLE_TEAM_ID:-4246K7Y6W2}"

fail() {
  printf 'Apple package check failed: %s\n' "$*" >&2
  exit 1
}

[[ -f "${package_script}" ]] || fail 'signed-only packaging script is missing'
grep -Fq -- '--keychain-profile' "${package_script}" || fail 'notarization profile is not required'
grep -Fq -- '--options runtime' "${package_script}" || fail 'Hardened Runtime is not required'
grep -Fq -- '--timestamp' "${package_script}" || fail 'secure timestamps are not required'
grep -Fq 'Developer ID Application:' "${package_script}" || fail 'Developer ID is not required'
grep -Fq 'Apple Distribution:' "${package_script}" || fail 'Apple Distribution is not required'
grep -Fq 'release-testing' "${package_script}" || fail 'iOS release-testing export is not required'
grep -Fq 'FlowSpliceTravelWidgets.appex' "${package_script}" || fail 'iOS Live Activity extension verification is not required'
grep -Fq 'stapler staple' "${package_script}" || fail 'macOS ticket stapling is not required'
grep -Fq 'hdiutil create' "${package_script}" || fail 'macOS DMG creation is not required'
grep -Fq 'context:primary-signature' "${package_script}" || fail 'macOS DMG Gatekeeper assessment is not required'
if grep -Fq 'mac_archive="${publication_dir}/${mac_name}.zip"' "${package_script}"; then
  fail 'macOS ZIP publication is forbidden; publish a DMG'
fi
if grep -Eq -- '--sign[[:space:]]+-|--timestamp=none' "${package_script}"; then
  fail 'identity-less signing is forbidden'
fi

if [[ $# -eq 0 ]]; then
  printf 'Apple signed-only package policy is consistent.\n'
  exit 0
fi
[[ $# -eq 1 && -d "$1" ]] || fail 'usage: check-apple-products.sh [release-directory]'
release_dir="$1"

cli_name="flowsplice-cli-${version}-macos-arm64"
mac_name="flowsplice-macos-${version}-arm64"
ios_name="flowsplice-ios-${version}-arm64"
required=(
  "${cli_name}.zip"
  "${cli_name}.notary.json"
  "${cli_name}.notary-log.json"
  "${mac_name}.dmg"
  "${mac_name}.notary.json"
  "${mac_name}.notary-log.json"
  "${ios_name}.ipa"
  "${ios_name}.provisioning.json"
  SHA256SUMS
)
for relative in "${required[@]}"; do
  [[ -s "${release_dir}/${relative}" ]] || fail "release member is missing: ${relative}"
done
[[ "$(find "${release_dir}" -maxdepth 1 -type f | wc -l | tr -d ' ')" == "${#required[@]}" ]] || fail 'release directory contains files outside the allowlist'
if find "${release_dir}" -type l -print -quit | grep -q .; then
  fail 'release directory contains a symbolic link'
fi
(
  cd "${release_dir}"
  shasum -a 256 -c SHA256SUMS
)
for receipt in "${cli_name}.notary.json" "${mac_name}.notary.json"; do
  [[ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "${release_dir}/${receipt}")" == 'Accepted' ]] \
    || fail "notarization receipt is not accepted: ${receipt}"
done

audit_root="$(mktemp -d "${TMPDIR:-/tmp}/flowsplice-apple-audit.XXXXXX")"
mac_mount="${audit_root}/mac-volume"
mac_mounted=false
cleanup() {
  if [[ "${mac_mounted}" == 'true' ]]; then
    hdiutil detach "${mac_mount}" >/dev/null 2>&1 || hdiutil detach -force "${mac_mount}" >/dev/null 2>&1 || true
  fi
  if [[ -d "${audit_root}" && "${audit_root}" == */flowsplice-apple-audit.* ]]; then
    rm -rf -- "${audit_root}"
  fi
}
trap cleanup EXIT

verify_developer_id() {
  local artifact="$1"
  local details
  codesign --verify --strict --verbose=2 "${artifact}"
  details="$(codesign -dvvv "${artifact}" 2>&1)"
  [[ "${details}" == *'Authority=Developer ID Application:'* ]] || fail "Developer ID authority is missing: ${artifact}"
  [[ "${details}" == *"TeamIdentifier=${team_id}"* ]] || fail "Developer ID team mismatch: ${artifact}"
  [[ "${details}" == *'Timestamp='* ]] || fail "secure timestamp is missing: ${artifact}"
  [[ "${details}" == *'Runtime Version='* ]] || fail "Hardened Runtime is missing: ${artifact}"
}

verify_developer_id_container() {
  local artifact="$1"
  local details
  codesign --verify --strict --verbose=2 "${artifact}"
  details="$(codesign -dvvv "${artifact}" 2>&1)"
  [[ "${details}" == *'Authority=Developer ID Application:'* ]] || fail "Developer ID authority is missing: ${artifact}"
  [[ "${details}" == *"TeamIdentifier=${team_id}"* ]] || fail "Developer ID team mismatch: ${artifact}"
  [[ "${details}" == *'Timestamp='* ]] || fail "secure timestamp is missing: ${artifact}"
}

verify_deployment_resource() {
  local resource="$1"
  if [[ -n "${FLOWSPLICE_PRIVATE_TRUST_FILE:-}" ]]; then
    python3 "${repo_root}/scripts/private-travel-trust.py" verify-root \
      --source "${FLOWSPLICE_PRIVATE_TRUST_FILE}" --artifact-resource "${resource}"
  elif [[ -e "${resource}" ]]; then
    fail 'deployment-neutral packages must not contain a deployment trust key'
  fi
}

ditto -x -k "${release_dir}/${cli_name}.zip" "${audit_root}/cli"
cli_root="${audit_root}/cli/${cli_name}"
[[ -d "${cli_root}" ]] || fail 'command-line archive root is missing'
cli_binaries=(flowsplice-server flowsplice-relay flowsplice-homeagent flowsplice-travelagent flowsplice-foobar flowsplice-trust)
for binary in "${cli_binaries[@]}"; do
  artifact="${cli_root}/bin/${binary}"
  [[ -x "${artifact}" ]] || fail "command-line binary is missing: ${binary}"
  [[ "$(lipo -archs "${artifact}")" == 'arm64' ]] || fail "command-line binary is not thin arm64: ${binary}"
  verify_developer_id "${artifact}"
done
(
  cd "${cli_root}"
  shasum -a 256 -c SHA256SUMS
)

mac_dmg="${release_dir}/${mac_name}.dmg"
verify_developer_id_container "${mac_dmg}"
xcrun stapler validate "${mac_dmg}"
hdiutil verify "${mac_dmg}"
spctl --assess --type open --context context:primary-signature --verbose=4 "${mac_dmg}"
mkdir -p "${mac_mount}"
hdiutil attach \
  -readonly \
  -nobrowse \
  -noautoopen \
  -mountpoint "${mac_mount}" \
  "${mac_dmg}" >/dev/null
mac_mounted=true
mac_app="${mac_mount}/FlowSplice.app"
[[ -d "${mac_app}" ]] || fail 'macOS DMG does not contain FlowSplice.app'
[[ -L "${mac_mount}/Applications" ]] || fail 'macOS DMG does not contain the Applications shortcut'
[[ "$(readlink "${mac_mount}/Applications")" == '/Applications' ]] || fail 'macOS DMG Applications shortcut has the wrong destination'
verify_developer_id "${mac_app}"
verify_deployment_resource "${mac_app}/Contents/Resources/bootstrap/deployment-root.pub"
[[ "$(lipo -archs "${mac_app}/Contents/MacOS/FlowSpliceMac")" == 'arm64' ]] || fail 'macOS app is not thin arm64'
[[ "$(plutil -extract CFBundleIdentifier raw -o - "${mac_app}/Contents/Info.plist")" == 'io.zxf.flowsplice.travel.macos' ]] || fail 'macOS bundle identifier is incorrect'
[[ "$(plutil -extract LSMinimumSystemVersion raw -o - "${mac_app}/Contents/Info.plist")" == '26.0' ]] || fail 'macOS minimum version is not 26.0'
[[ -f "${mac_app}/Contents/Resources/PrivacyInfo.xcprivacy" ]] || fail 'macOS privacy manifest is missing'
xcrun stapler validate "${mac_app}"
spctl --assess --type execute --verbose=4 "${mac_app}"
if strings "${mac_app}/Contents/MacOS/FlowSpliceMac" | grep -Eq 'FLOWSPLICE_(E2E|UI_TEST)|--real-backend'; then
  fail 'macOS Release executable contains an automated-test control surface'
fi
hdiutil detach "${mac_mount}" >/dev/null
mac_mounted=false

mkdir -p "${audit_root}/ios"
unzip -q "${release_dir}/${ios_name}.ipa" -d "${audit_root}/ios"
ios_app="$(find "${audit_root}/ios/Payload" -maxdepth 1 -type d -name '*.app' -print -quit)"
[[ -n "${ios_app}" ]] || fail 'IPA application bundle is missing'
ios_executable="${ios_app}/$(plutil -extract CFBundleExecutable raw -o - "${ios_app}/Info.plist")"
codesign --verify --strict --verbose=2 "${ios_app}"
verify_deployment_resource "${ios_app}/bootstrap/deployment-root.pub"
ios_details="$(codesign -dvvv "${ios_app}" 2>&1)"
[[ "${ios_details}" == *'Authority=Apple Distribution:'* ]] || fail 'IPA does not use Apple Distribution signing'
[[ "${ios_details}" == *"TeamIdentifier=${team_id}"* ]] || fail 'IPA team mismatch'
[[ "$(lipo -archs "${ios_executable}")" == 'arm64' ]] || fail 'IPA executable is not thin arm64'
[[ "$(plutil -extract CFBundleIdentifier raw -o - "${ios_app}/Info.plist")" == 'io.zxf.flowsplice.travel' ]] || fail 'IPA bundle identifier is incorrect'
[[ "$(plutil -extract MinimumOSVersion raw -o - "${ios_app}/Info.plist")" == '17.0' ]] || fail 'IPA minimum OS version is not 17.0'
[[ "$(plutil -extract NSSupportsLiveActivities raw -o - "${ios_app}/Info.plist")" == 'true' ]] || fail 'IPA does not declare Live Activity support'
[[ -f "${ios_app}/PrivacyInfo.xcprivacy" ]] || fail 'IPA privacy manifest is missing'
[[ -f "${ios_app}/embedded.mobileprovision" ]] || fail 'IPA distribution profile is missing'
python3 - "${ios_app}/Info.plist" <<'PY' || fail 'IPA background or URL policy is invalid'
import plistlib
import sys

with open(sys.argv[1], "rb") as handle:
    info = plistlib.load(handle)
if info.get("UIBackgroundModes") != ["audio"]:
    raise SystemExit("only audio background mode is allowed")
if "CFBundleURLTypes" in info:
    raise SystemExit("custom URL schemes are forbidden")
PY

ios_live_activity="${ios_app}/PlugIns/FlowSpliceTravelWidgets.appex"
[[ -d "${ios_live_activity}" ]] || fail 'IPA Live Activity extension is missing'
ios_live_activity_executable="${ios_live_activity}/$(plutil -extract CFBundleExecutable raw -o - "${ios_live_activity}/Info.plist")"
codesign --verify --strict --verbose=2 "${ios_live_activity}"
ios_live_activity_details="$(codesign -dvvv "${ios_live_activity}" 2>&1)"
[[ "${ios_live_activity_details}" == *'Authority=Apple Distribution:'* ]] || fail 'Live Activity extension does not use Apple Distribution signing'
[[ "${ios_live_activity_details}" == *"TeamIdentifier=${team_id}"* ]] || fail 'Live Activity extension team mismatch'
[[ "$(lipo -archs "${ios_live_activity_executable}")" == 'arm64' ]] || fail 'Live Activity extension is not thin arm64'
[[ "$(plutil -extract CFBundleIdentifier raw -o - "${ios_live_activity}/Info.plist")" == 'io.zxf.flowsplice.travel.widgets' ]] || fail 'Live Activity extension bundle identifier is incorrect'
[[ "$(plutil -extract NSExtension.NSExtensionPointIdentifier raw -o - "${ios_live_activity}/Info.plist")" == 'com.apple.widgetkit-extension' ]] || fail 'Live Activity extension point is incorrect'
[[ -f "${ios_live_activity}/embedded.mobileprovision" ]] || fail 'Live Activity extension distribution profile is missing'
otool -L "${ios_executable}" | grep -Fq '/ActivityKit.framework/ActivityKit' || fail 'IPA application is not linked to ActivityKit'
otool -L "${ios_live_activity_executable}" | grep -Fq '/ActivityKit.framework/ActivityKit' || fail 'Live Activity extension is not linked to ActivityKit'
if find "${ios_app}" \( -name '*.xctest' -o -name '*UITests*' -o -name '*Tests*' \) -print -quit | grep -q .; then
  fail 'IPA contains a test bundle'
fi
if strings "${ios_executable}" | grep -Eq 'FLOWSPLICE_(E2E|UI_TEST)|--flowsplice-(apple-)?e2e'; then
  fail 'IPA Release executable contains an automated-test control surface'
fi

printf 'Apple signed release verified: %s\n' "${release_dir}"
