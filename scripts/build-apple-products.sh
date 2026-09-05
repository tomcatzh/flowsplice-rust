#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${repo_root}/Cargo.toml" | head -n 1)"
team_id="${FLOWSPLICE_APPLE_TEAM_ID:-4246K7Y6W2}"
notary_profile="${FLOWSPLICE_NOTARY_PROFILE:-flowsplice-notary}"
ios_device_selector="${FLOWSPLICE_IOS_DEVICE:-iPad}"
developer_dir="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
release_parent="${repo_root}/dist/apple"
release_dir="${FLOWSPLICE_APPLE_OUTPUT_DIR:-${release_parent}/${version}}"
release_parent="$(dirname -- "${release_dir}")"
private_trust_file="${FLOWSPLICE_PRIVATE_TRUST_FILE:-}"
private_stage_parent="${FLOWSPLICE_APPLE_STAGE_PARENT:-${TMPDIR:-/tmp}}"
stage_root=''

fail() {
  printf 'Apple release failed: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [[ -n "${stage_root}" && -d "${stage_root}" && "${stage_root}" == */flowsplice-apple-release.* ]]; then
    rm -rf -- "${stage_root}"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ -n "${private_trust_file}" ]]; then
  python3 "${repo_root}/scripts/private-travel-trust.py" check-path --path "${release_dir}"
  python3 "${repo_root}/scripts/private-travel-trust.py" check-path --path "${private_stage_parent}"
fi

[[ -n "${version}" ]] || fail 'workspace version is unavailable'
[[ -d "${developer_dir}" ]] || fail "Xcode developer directory is missing: ${developer_dir}"
[[ "$(uname -m)" == 'arm64' ]] || fail 'Apple release packaging requires an Apple-silicon Mac'
[[ ! -e "${release_dir}" ]] || fail "the signed release already exists: ${release_dir}"

for command in cargo codesign ditto file hdiutil lipo ln npm otool plutil python3 security shasum spctl unzip xattr xcodebuild xcrun; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command is missing: ${command}"
done

export DEVELOPER_DIR="${developer_dir}"

resolve_identity() {
  local prefix="$1"
  local matches
  matches="$(security find-identity -v -p codesigning | awk -v prefix="${prefix}" -v team="${team_id}" '
    index($0, "\"" prefix) && index($0, "(" team ")\"") { print $2 }
  ')"
  if [[ "$(printf '%s\n' "${matches}" | sed '/^$/d' | wc -l | tr -d ' ')" != '1' ]]; then
    fail "expected exactly one ${prefix} identity for team ${team_id}"
  fi
  printf '%s\n' "${matches}"
}

developer_id_hash="$(resolve_identity 'Developer ID Application:')"
distribution_hash="$(resolve_identity 'Apple Distribution:')"

# Credential access and Apple service availability are mandatory before any build begins.
xcrun notarytool history \
  --keychain-profile "${notary_profile}" \
  --output-format json >/dev/null

stage_root="$(mktemp -d "${private_stage_parent}/flowsplice-apple-release.XXXXXX")"
chmod 700 "${stage_root}"
publication_dir="${stage_root}/publication"
cargo_target_dir="${stage_root}/cargo-target"
mkdir -p "${publication_dir}" "${cargo_target_dir}"
export CARGO_TARGET_DIR="${cargo_target_dir}"
trust_xcode_args=("FLOWSPLICE_PRIVATE_TRUST_FILE=")
if [[ -n "${private_trust_file}" ]]; then
  frozen_trust="${stage_root}/trust/deployment-root.pub"
  python3 "${repo_root}/scripts/private-travel-trust.py" copy-root \
    --source "${private_trust_file}" --destination "${frozen_trust}"
  export FLOWSPLICE_PRIVATE_TRUST_FILE="${frozen_trust}"
  printf '%s\n' "${frozen_trust}" >"${stage_root}/trust-inputs.xcfilelist"
  trust_xcode_args+=("FLOWSPLICE_PRIVATE_TRUST_FILE=${frozen_trust}" "FLOWSPLICE_TRUST_INPUT_LIST=${stage_root}/trust-inputs.xcfilelist")
fi

device_json="${stage_root}/devices.json"
xcrun devicectl list devices --json-output "${device_json}" --quiet
device_fields="$(python3 - "${device_json}" "${ios_device_selector}" <<'PY'
import json
import sys

payload = json.load(open(sys.argv[1], encoding="utf-8"))
selector = sys.argv[2]
devices = payload.get("result", {}).get("devices", [])
matches = []
for device in devices:
    props = device.get("deviceProperties", {})
    hardware = device.get("hardwareProperties", {})
    connection = device.get("connectionProperties", {})
    values = {
        device.get("identifier", ""),
        props.get("name", ""),
        hardware.get("udid", ""),
        hardware.get("serialNumber", ""),
    }
    if selector in values and connection.get("pairingState") == "paired":
        matches.append(device)
if len(matches) != 1:
    raise SystemExit(f"expected one paired iOS device matching {selector!r}, found {len(matches)}")
device = matches[0]
if device.get("deviceProperties", {}).get("developerModeStatus") != "enabled":
    raise SystemExit("the selected iOS device does not have Developer Mode enabled")
print(device["identifier"])
print(device["hardwareProperties"]["udid"])
PY
)"
ios_core_device_id="$(printf '%s\n' "${device_fields}" | sed -n '1p')"
ios_device_udid="$(printf '%s\n' "${device_fields}" | sed -n '2p')"
[[ -n "${ios_core_device_id}" && -n "${ios_device_udid}" ]] || fail 'paired iOS device metadata is incomplete'

verify_developer_id_signature() {
  local artifact="$1"
  local details
  codesign --verify --strict --verbose=2 "${artifact}"
  details="$(codesign -dvvv "${artifact}" 2>&1)"
  [[ "${details}" == *"Authority=Developer ID Application:"* ]] || fail "Developer ID authority is missing: ${artifact}"
  [[ "${details}" == *"TeamIdentifier=${team_id}"* ]] || fail "Developer ID team mismatch: ${artifact}"
  [[ "${details}" == *'Timestamp='* ]] || fail "secure timestamp is missing: ${artifact}"
  [[ "${details}" == *'Runtime Version='* ]] || fail "Hardened Runtime is missing: ${artifact}"
}

verify_developer_id_container_signature() {
  local artifact="$1"
  local details
  codesign --verify --strict --verbose=2 "${artifact}"
  details="$(codesign -dvvv "${artifact}" 2>&1)"
  [[ "${details}" == *"Authority=Developer ID Application:"* ]] || fail "Developer ID authority is missing: ${artifact}"
  [[ "${details}" == *"TeamIdentifier=${team_id}"* ]] || fail "Developer ID team mismatch: ${artifact}"
  [[ "${details}" == *'Timestamp='* ]] || fail "secure timestamp is missing: ${artifact}"
}

notarize_archive() {
  local archive="$1"
  local receipt="$2"
  local log_file="$3"
  local attempt
  local status
  local submission_id

  for attempt in 1 2 3 4; do
    : >"${receipt}"
    if xcrun notarytool submit "${archive}" \
      --keychain-profile "${notary_profile}" \
      --wait \
      --output-format json >"${receipt}"; then
      break
    fi
    if [[ "${attempt}" == '4' ]]; then
      [[ ! -s "${receipt}" ]] || sed -n '1,200p' "${receipt}" >&2
      fail "notarization submission failed after ${attempt} attempts: ${archive}"
    fi
    printf 'Notarization attempt %s failed; retrying...\n' "${attempt}" >&2
    sleep "$((attempt * 5))"
  done
  status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "${receipt}")"
  submission_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "${receipt}")"
  if [[ "${status}" != 'Accepted' ]]; then
    xcrun notarytool log "${submission_id}" \
      --keychain-profile "${notary_profile}" || true
    fail "Apple notarization was not accepted for ${archive}: ${status}"
  fi
  xcrun notarytool log "${submission_id}" "${log_file}" \
    --keychain-profile "${notary_profile}"
}

printf 'Building signed macOS command-line bundle...\n'
(cd "${repo_root}/travelagent/web" && npm run build)
(cd "${repo_root}/homeagent/web" && npm run build)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-server \
  -p flowsplice-relay \
  -p flowsplice-homeagent \
  -p flowsplice-travelagent \
  -p flowsplice-foobar)
(cd "${repo_root}" && cargo build --locked --release \
  -p flowsplice-enrollment --bin flowsplice-trust)
bash "${repo_root}/tests/check-release-feature-gates.sh" \
  --home "${cargo_target_dir}/release/flowsplice-homeagent" \
  --travel "${cargo_target_dir}/release/flowsplice-travelagent"

cli_name="flowsplice-cli-${version}-macos-arm64"
cli_root="${stage_root}/${cli_name}"
mkdir -p "${cli_root}/bin" "${cli_root}/docs"
cp "${repo_root}/LICENSE" "${cli_root}/LICENSE"
cp "${repo_root}/README.md" "${cli_root}/README.md"
cp "${repo_root}/docs/QUICK_START.zh-CN.md" "${cli_root}/docs/QUICK_START.zh-CN.md"
cp "${repo_root}/docs/HOME2_QUICK_START.zh-CN.md" "${cli_root}/docs/HOME2_QUICK_START.zh-CN.md"

cli_binaries=(
  flowsplice-server
  flowsplice-relay
  flowsplice-homeagent
  flowsplice-travelagent
  flowsplice-foobar
  flowsplice-trust
)
for binary in "${cli_binaries[@]}"; do
  source_binary="${cargo_target_dir}/release/${binary}"
  destination_binary="${cli_root}/bin/${binary}"
  [[ -x "${source_binary}" ]] || fail "built command is missing: ${source_binary}"
  cp "${source_binary}" "${destination_binary}"
  chmod 755 "${destination_binary}"
  [[ "$(lipo -archs "${destination_binary}")" == 'arm64' ]] || fail "command is not thin arm64: ${binary}"
  codesign \
    --force \
    --sign "${developer_id_hash}" \
    --identifier "io.zxf.flowsplice.${binary#flowsplice-}" \
    --options runtime \
    --timestamp \
    "${destination_binary}"
  verify_developer_id_signature "${destination_binary}"
done
(
  cd "${cli_root}"
  find bin docs -type f -print | LC_ALL=C sort | while IFS= read -r path; do
    shasum -a 256 "${path}"
  done
  shasum -a 256 LICENSE README.md
) >"${cli_root}/SHA256SUMS"
cli_archive="${publication_dir}/${cli_name}.zip"
ditto -c -k --keepParent --sequesterRsrc "${cli_root}" "${cli_archive}"
cli_receipt="${publication_dir}/${cli_name}.notary.json"
cli_notary_log="${publication_dir}/${cli_name}.notary-log.json"
notarize_archive "${cli_archive}" "${cli_receipt}" "${cli_notary_log}"

printf 'Building signed and notarized macOS desktop app...\n'
mac_derived="${stage_root}/mac-derived"
xcodebuild \
  -project "${repo_root}/travel-macos/FlowSpliceMac/FlowSpliceMac.xcodeproj" \
  -scheme FlowSpliceMac \
  -configuration Release \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath "${mac_derived}" \
  CODE_SIGNING_ALLOWED=NO \
  "${trust_xcode_args[@]}" \
  "LIBRARY_SEARCH_PATHS=${cargo_target_dir}/aarch64-apple-darwin/release" \
  build
mac_built_app="${mac_derived}/Build/Products/Release/FlowSpliceMac.app"
[[ -d "${mac_built_app}" ]] || fail 'Xcode did not produce FlowSpliceMac.app'
mac_stage_app="${stage_root}/FlowSplice.app"
ditto "${mac_built_app}" "${mac_stage_app}"
if [[ -n "${private_trust_file}" ]]; then
  python3 "${repo_root}/scripts/private-travel-trust.py" verify-root \
    --source "${frozen_trust}" --artifact-resource "${mac_stage_app}/Contents/Resources/bootstrap/deployment-root.pub"
fi
xattr -cr "${mac_stage_app}"
codesign \
  --force \
  --sign "${developer_id_hash}" \
  --identifier io.zxf.flowsplice.travel.macos \
  --options runtime \
  --timestamp \
  "${mac_stage_app}"
verify_developer_id_signature "${mac_stage_app}"
[[ "$(lipo -archs "${mac_stage_app}/Contents/MacOS/FlowSpliceMac")" == 'arm64' ]] || fail 'macOS app is not thin arm64'
[[ "$(plutil -extract CFBundleIdentifier raw -o - "${mac_stage_app}/Contents/Info.plist")" == 'io.zxf.flowsplice.travel.macos' ]] || fail 'macOS bundle identifier is incorrect'
[[ "$(plutil -extract LSMinimumSystemVersion raw -o - "${mac_stage_app}/Contents/Info.plist")" == '26.0' ]] || fail 'macOS minimum version is not 26.0'
mac_submission="${stage_root}/flowsplice-macos-notary-submission.zip"
ditto -c -k --keepParent --sequesterRsrc "${mac_stage_app}" "${mac_submission}"
mac_name="flowsplice-macos-${version}-arm64"
mac_app_receipt="${stage_root}/${mac_name}-app.notary.json"
mac_app_notary_log="${stage_root}/${mac_name}-app.notary-log.json"
notarize_archive "${mac_submission}" "${mac_app_receipt}" "${mac_app_notary_log}"
xcrun stapler staple "${mac_stage_app}"
xcrun stapler validate "${mac_stage_app}"
spctl --assess --type execute --verbose=4 "${mac_stage_app}"
verify_developer_id_signature "${mac_stage_app}"

mac_dmg_root="${stage_root}/mac-dmg-root"
mkdir -p "${mac_dmg_root}"
ditto "${mac_stage_app}" "${mac_dmg_root}/FlowSplice.app"
ln -s /Applications "${mac_dmg_root}/Applications"
mac_dmg="${publication_dir}/${mac_name}.dmg"
hdiutil create \
  -srcfolder "${mac_dmg_root}" \
  -volname FlowSplice \
  -format UDZO \
  -imagekey zlib-level=9 \
  -ov \
  "${mac_dmg}"
codesign \
  --force \
  --sign "${developer_id_hash}" \
  --timestamp \
  "${mac_dmg}"
verify_developer_id_container_signature "${mac_dmg}"
mac_receipt="${publication_dir}/${mac_name}.notary.json"
mac_notary_log="${publication_dir}/${mac_name}.notary-log.json"
notarize_archive "${mac_dmg}" "${mac_receipt}" "${mac_notary_log}"
xcrun stapler staple "${mac_dmg}"
xcrun stapler validate "${mac_dmg}"
hdiutil verify "${mac_dmg}"
spctl --assess --type open --context context:primary-signature --verbose=4 "${mac_dmg}"
verify_developer_id_container_signature "${mac_dmg}"

printf 'Building Apple Distribution-signed iOS package...\n'
ios_archive="${stage_root}/FlowSpliceTravel.xcarchive"
xcodebuild \
  -project "${repo_root}/travel-apple/FlowSpliceTravel/FlowSpliceTravel.xcodeproj" \
  -scheme FlowSpliceTravel \
  -configuration Release \
  -destination 'generic/platform=iOS' \
  -archivePath "${ios_archive}" \
  -derivedDataPath "${stage_root}/ios-derived" \
  "${trust_xcode_args[@]}" \
  -allowProvisioningUpdates \
  -allowProvisioningDeviceRegistration \
  DEVELOPMENT_TEAM="${team_id}" \
  CODE_SIGN_STYLE=Automatic \
  "LIBRARY_SEARCH_PATHS=${cargo_target_dir}/aarch64-apple-ios/release" \
  archive

export_options="${stage_root}/ExportOptions.plist"
plutil -create xml1 "${export_options}"
plutil -insert method -string release-testing "${export_options}"
plutil -insert destination -string export "${export_options}"
plutil -insert signingStyle -string automatic "${export_options}"
plutil -insert teamID -string "${team_id}" "${export_options}"
plutil -insert stripSwiftSymbols -bool true "${export_options}"
plutil -insert thinning -string '<none>' "${export_options}"

ios_export="${stage_root}/ios-export"
xcodebuild \
  -exportArchive \
  -archivePath "${ios_archive}" \
  -exportPath "${ios_export}" \
  -exportOptionsPlist "${export_options}" \
  -allowProvisioningUpdates
source_ipa="$(find "${ios_export}" -maxdepth 1 -type f -name '*.ipa' -print -quit)"
[[ -n "${source_ipa}" ]] || fail 'Xcode did not export an IPA'
ios_name="flowsplice-ios-${version}-arm64"
ios_ipa="${publication_dir}/${ios_name}.ipa"
cp "${source_ipa}" "${ios_ipa}"

ios_extract="${stage_root}/ios-package"
mkdir -p "${ios_extract}"
unzip -q "${ios_ipa}" -d "${ios_extract}"
ios_app="$(find "${ios_extract}/Payload" -maxdepth 1 -type d -name '*.app' -print -quit)"
[[ -n "${ios_app}" ]] || fail 'IPA does not contain an application bundle'
ios_executable="${ios_app}/$(plutil -extract CFBundleExecutable raw -o - "${ios_app}/Info.plist")"
ios_profile="${ios_app}/embedded.mobileprovision"
ios_profile_plist="${stage_root}/ios-profile.plist"
[[ -f "${ios_profile}" ]] || fail 'IPA does not contain a distribution provisioning profile'
security cms -D -i "${ios_profile}" >"${ios_profile_plist}"
codesign --verify --strict --verbose=2 "${ios_app}"
ios_signature="$(codesign -dvvv "${ios_app}" 2>&1)"
[[ "${ios_signature}" == *'Authority=Apple Distribution:'* ]] || fail 'IPA does not use Apple Distribution signing'
[[ "${ios_signature}" == *"TeamIdentifier=${team_id}"* ]] || fail 'IPA signing team is incorrect'
[[ "$(lipo -archs "${ios_executable}")" == 'arm64' ]] || fail 'IPA executable is not thin arm64'
[[ "$(plutil -extract CFBundleIdentifier raw -o - "${ios_app}/Info.plist")" == 'io.zxf.flowsplice.travel' ]] || fail 'iOS bundle identifier is incorrect'
[[ "$(plutil -extract MinimumOSVersion raw -o - "${ios_app}/Info.plist")" == '17.0' ]] || fail 'iOS minimum version is not 17.0'
[[ "$(plutil -extract NSSupportsLiveActivities raw -o - "${ios_app}/Info.plist")" == 'true' ]] || fail 'iOS app does not declare Live Activity support'
[[ -f "${ios_app}/PrivacyInfo.xcprivacy" ]] || fail 'IPA privacy manifest is missing'
python3 - "${ios_app}/Info.plist" <<'PY' || fail 'iOS background or URL policy is invalid'
import plistlib
import sys

with open(sys.argv[1], "rb") as handle:
    info = plistlib.load(handle)
if info.get("UIBackgroundModes") != ["audio"]:
    raise SystemExit("only audio background mode is allowed")
if "CFBundleURLTypes" in info:
    raise SystemExit("custom URL schemes are forbidden")
PY
if strings "${ios_executable}" | grep -Eq 'FLOWSPLICE_(E2E|UI_TEST)|--flowsplice-(apple-)?e2e'; then
  fail 'IPA executable contains an automated-test control surface'
fi

ios_live_activity="${ios_app}/PlugIns/FlowSpliceTravelWidgets.appex"
[[ -d "${ios_live_activity}" ]] || fail 'IPA does not contain the Live Activity extension'
ios_live_activity_executable="${ios_live_activity}/$(plutil -extract CFBundleExecutable raw -o - "${ios_live_activity}/Info.plist")"
ios_live_activity_profile="${ios_live_activity}/embedded.mobileprovision"
ios_live_activity_profile_plist="${stage_root}/ios-live-activity-profile.plist"
[[ -f "${ios_live_activity_profile}" ]] || fail 'Live Activity extension does not contain a distribution provisioning profile'
security cms -D -i "${ios_live_activity_profile}" >"${ios_live_activity_profile_plist}"
codesign --verify --strict --verbose=2 "${ios_live_activity}"
ios_live_activity_signature="$(codesign -dvvv "${ios_live_activity}" 2>&1)"
[[ "${ios_live_activity_signature}" == *'Authority=Apple Distribution:'* ]] || fail 'Live Activity extension does not use Apple Distribution signing'
[[ "${ios_live_activity_signature}" == *"TeamIdentifier=${team_id}"* ]] || fail 'Live Activity extension signing team is incorrect'
[[ "$(lipo -archs "${ios_live_activity_executable}")" == 'arm64' ]] || fail 'Live Activity extension is not thin arm64'
[[ "$(plutil -extract CFBundleIdentifier raw -o - "${ios_live_activity}/Info.plist")" == 'io.zxf.flowsplice.travel.widgets' ]] || fail 'Live Activity extension bundle identifier is incorrect'
[[ "$(plutil -extract NSExtension.NSExtensionPointIdentifier raw -o - "${ios_live_activity}/Info.plist")" == 'com.apple.widgetkit-extension' ]] || fail 'Live Activity extension point is incorrect'
otool -L "${ios_executable}" | grep -Fq '/ActivityKit.framework/ActivityKit' || fail 'iOS application is not linked to ActivityKit'
otool -L "${ios_live_activity_executable}" | grep -Fq '/ActivityKit.framework/ActivityKit' || fail 'Live Activity extension is not linked to ActivityKit'

python3 - "${ios_device_udid}" "${team_id}" "${ios_profile_plist}" "${ios_live_activity_profile_plist}" <<'PY'
import plistlib
import sys
from datetime import datetime, timezone

udid = sys.argv[1]
team = sys.argv[2]
for path in sys.argv[3:]:
    with open(path, "rb") as handle:
        profile = plistlib.load(handle)
    if team not in profile.get("TeamIdentifier", []):
        raise SystemExit(f"provisioning profile team mismatch: {path}")
    if udid not in profile.get("ProvisionedDevices", []):
        raise SystemExit(f"selected device is absent from the release-testing profile: {path}")
    expiration = profile.get("ExpirationDate")
    if expiration is None or expiration.replace(tzinfo=timezone.utc) <= datetime.now(timezone.utc):
        raise SystemExit(f"provisioning profile is expired: {path}")
    if profile.get("Entitlements", {}).get("get-task-allow") is not False:
        raise SystemExit(f"release-testing profile unexpectedly enables get-task-allow: {path}")
PY

ios_metadata="${publication_dir}/${ios_name}.provisioning.json"
python3 - "${ios_device_udid}" "${ios_profile_plist}" "${ios_live_activity_profile_plist}" >"${ios_metadata}" <<'PY'
import json
import plistlib
import sys

def summary(path):
    with open(path, "rb") as handle:
        profile = plistlib.load(handle)
    return {
        "name": profile.get("Name"),
        "uuid": profile.get("UUID"),
        "team_identifiers": profile.get("TeamIdentifier", []),
        "application_identifier": profile.get("Entitlements", {}).get("application-identifier"),
        "expiration": profile.get("ExpirationDate").isoformat(),
        "provisioned_devices": profile.get("ProvisionedDevices", []),
        "get_task_allow": profile.get("Entitlements", {}).get("get-task-allow"),
    }

application = summary(sys.argv[2])
live_activity = summary(sys.argv[3])
result = dict(application)
result["verified_device"] = sys.argv[1]
result["bundles"] = {
    "io.zxf.flowsplice.travel": application,
    "io.zxf.flowsplice.travel.widgets": live_activity,
}
json.dump(result, sys.stdout, indent=2, sort_keys=True)
print()
PY

# A signed IPA is not accepted until its exact exported app installs and launches on a listed device.
install_json="${stage_root}/ios-install.json"
launch_json="${stage_root}/ios-launch.json"
launch_error="${stage_root}/ios-launch.stderr"
xcrun devicectl device install app \
  --device "${ios_core_device_id}" \
  "${ios_app}" \
  --json-output "${install_json}" \
  --quiet
launch_succeeded=false
for launch_attempt in {1..120}; do
  : >"${launch_error}"
  if xcrun devicectl device process launch \
    --device "${ios_core_device_id}" \
    --terminate-existing \
    io.zxf.flowsplice.travel \
    --json-output "${launch_json}" \
    --quiet 2> >(tee "${launch_error}" >&2); then
    launch_succeeded=true
    break
  fi
  if ! grep -Eq 'Locked|could not be, unlocked' "${launch_error}"; then
    fail 'the Apple Distribution-signed iOS app failed to launch'
  fi
  if [[ "${launch_attempt}" == '120' ]]; then
    fail 'the iOS device remained locked for ten minutes'
  fi
  printf 'The iOS device is locked; unlock it to continue release verification (%s/120).\n' "${launch_attempt}" >&2
  sleep 5
done
[[ "${launch_succeeded}" == 'true' ]] || fail 'the iOS launch verification did not complete'

(
  cd "${publication_dir}"
  find . -maxdepth 1 -type f ! -name SHA256SUMS -print \
    | sed 's#^./##' \
    | LC_ALL=C sort \
    | while IFS= read -r path; do shasum -a 256 "${path}"; done
) >"${publication_dir}/SHA256SUMS"

bash "${repo_root}/tests/check-apple-products.sh" "${publication_dir}"

mkdir -p "${release_parent}"
mv "${publication_dir}" "${release_dir}"
printf 'Signed Apple release published: %s\n' "${release_dir}"
printf 'Verified iOS device: %s\n' "${ios_device_udid}"
