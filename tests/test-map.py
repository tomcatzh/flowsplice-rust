#!/usr/bin/env python3
"""Tag every FlowSplice test point with a level (horizontal) and modules (vertical).

Writes tests/test-map.csv; with --check, fails when that file is out of date or when the E2E module
groups in tests/e2e/run.sh and tests/e2e/assert_e2e.py no longer match this list. Levels and
modules are defined in docs/TESTING.md.
"""
import ast
import csv
import io
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CHECK = sys.argv[1:] == ["--check"]
rows = []


def add(test_id, level, modules, platform, runner, location, note=""):
    rows.append({"id": test_id, "level": level, "modules": " ".join(modules), "platform": platform,
                 "runner": runner, "location": location, "note": note})


# --- Rust tests ---------------------------------------------------------------------------------
FILE_MODULES = {
    "internal/flowsplice-core/src/frame.rs": ["WIRE"],
    "internal/flowsplice-core/src/tls.rs": ["WIRE"],
    "internal/flowsplice-core/src/deployment.rs": ["TRUST"],
    "internal/flowsplice-core/src/config.rs": ["TRUST"],
    "internal/flowsplice-core/src/carrier.rs": ["CARRIER"],
    "internal/flowsplice-core/src/route.rs": ["CARRIER"],
    "internal/flowsplice-core/src/authorization.rs": ["AUTHZ"],
    "internal/flowsplice-core/src/statistics.rs": ["STATS"],
    "internal/flowsplice-core/tests/business.rs": ["AUTHZ", "ENROLL"],
    "internal/flowsplice-core/tests/service_class.rs": ["CLASS"],
    "internal/flowsplice-enrollment/src/bin/flowsplice-trust.rs": ["TRUST"],
    "internal/flowsplice-enrollment/tests/service_class.rs": ["CLASS", "ENROLL"],
    "internal/flowsplice-transport/src/services.rs": ["RUNTIME", "MAPPING"],
    "crates/flowsplice-travel-core/src/tcp_flow.rs": ["CARRIER"],
    "crates/flowsplice-travel-core/src/carrier_regressions.rs": ["CARRIER"],
    "crates/flowsplice-travel-core/src/udp_regressions.rs": ["UDP"],
    "crates/flowsplice-travel-core/src/socket.rs": ["SDK"],
    "crates/flowsplice-travel-core/src/installation_paths.rs": ["ENROLL", "SDK"],
    "crates/flowsplice-travel-core/src/service_class_tests.rs": ["CLASS"],
    "crates/flowsplice-home-core/src/tcp_flow.rs": ["CARRIER"],
    "server/src/backpressure_tests.rs": ["CONTROL"],
    "server/src/authorization.rs": ["AUTHZ"],
    "server/src/authorization_business_tests.rs": ["AUTHZ"],
    "server/src/service_class_tests.rs": ["CLASS"],
    "relay/src/requests.rs": ["CONTROL"],
    "relay/src/openwrt.rs": ["PACKAGING"],
    "homeagent/src/issuance_ledger.rs": ["ENROLL"],
    "travel-apple/rust/src/lib.rs": ["IOS", "MACOS", "SDK"],
    "travel-android/rust/src/lib.rs": ["ANDROID", "SDK"],
    "foobar/src/main.rs": ["TOOLS"],
}
PREFIX_MODULES = [
    ("internal/flowsplice-enrollment/", ["ENROLL"]),
    ("internal/flowsplice-transport/", ["RUNTIME"]),
    ("internal/flowsplice-storage/", ["RUNTIME"]),
    ("crates/flowsplice-home-core/", ["RUNTIME"]),
    ("tests/fixtures/", ["TOOLS"]),
]
# Name keywords for files whose tests cover several modules (first match wins per file).
NAME_RULES = {
    "crates/flowsplice-travel-core/src/lib.rs": [
        (r"enrollment|outbox", ["ENROLL"]), (r"mapping", ["MAPPING"]), (r"travel_ui", ["LOCALUI"]),
        (r"control_trust|rollback", ["AUTHZ"]), (r"configured_home", ["TRUST"]), (r"diagnostics|online_status", ["DIAG"]),
        (r"seed|relay|address_overrides", ["CONTROL"]),
    ],
    "server/src/main.rs": [
        (r"statistics", ["STATS"]), (r"session|login|lease|release", ["CONTROL"]), (r"work_readiness", ["CARRIER"]),
        (r"catalog|home_registry|home_without_services", ["MAPPING"]), (r"configured_homes", ["TRUST"]), (r"relay", ["CONTROL"]),
    ],
    "relay/src/main.rs": [(r"direct_route|route_replay", ["CARRIER"]), (r".", ["CONTROL"])],
    "homeagent/src/main.rs": [(r"issuer_ui", ["LOCALUI", "ENROLL"]), (r"secondary_home", ["ENROLL"]), (r".", ["ENROLL"])],
    "crates/flowsplice-travel-core/src/review_regressions.rs": [
        (r"data_address", ["CARRIER"]), (r"^r01", ["TRUST"]), (r"^r02|^r11", ["RUNTIME", "SDK"]), (r"^r04", ["MAPPING"]),
        (r"^r06", ["STATS"]), (r"^r08", ["CONTROL"]), (r"^r09|^r10|^r12", ["SDK"]),
    ],
    "internal/flowsplice-core/src/protocol.rs": [
        (r"carrier_race", ["CARRIER"]), (r"bootstrap", ["TRUST"]), (r"catalog|route_request", ["MAPPING"]),
        (r"relay_endpoint|travel_hello", ["CONTROL"]), (r"snapshot", ["AUTHZ"]), (r".", ["WIRE"]),
    ],
    "internal/flowsplice-storage/src/lib.rs": [(r"statistics|metric", ["STATS"]), (r".", ["RUNTIME"])],
}
L2_FILES = re.compile(r"/tests/|_regressions\.rs$")

files = subprocess.run(["git", "-C", str(REPO), "grep", "-lE", r"#\[(tokio::)?test", "--", "*.rs"],
                       capture_output=True, text=True, check=True).stdout.split()
for path in files:
    text = (REPO / path).read_text()
    for match in re.finditer(r"#\[(?:tokio::)?test[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+([a-z0-9_]+)", text):
        name = match.group(1)
        modules = None
        for pattern, mods in NAME_RULES.get(path, []):
            if re.search(pattern, name):
                modules = mods
                break
        if modules is None:
            modules = FILE_MODULES.get(path) or next((m for p, m in PREFIX_MODULES if path.startswith(p)), ["UNSORTED"])
        level = "L2" if L2_FILES.search(path) else "L1"
        add(f"rust:{name}", level, modules, "host", "cargo test --workspace", path)

# --- Docker E2E (tests/e2e/assert_e2e.py named checks) -------------------------------------------
E2E = {
    "ipv4-ipv6-loopback-ui-policy": ["LOCALUI"], "travel-runtime-mapping-hot-rebind-and-restart-persistence": ["MAPPING"],
    "concurrent-mapping-updates-serialized": ["MAPPING"], "authorization-state-missing-fails-closed": ["AUTHZ"],
    "server-downlinked-two-relay-spki-directory": ["CONTROL"], "two-home-catalog": ["MAPPING"],
    "same-service-id-isolated-by-home": ["MAPPING"], "logical-business-selects-exact-home": ["MAPPING"],
    "wrong-home-service-fails-without-fallback": ["MAPPING"], "target-failure-isolated-per-home": ["MAPPING", "CARRIER"],
    "one-home-offline-does-not-affect-other-home": ["MAPPING", "CARRIER"], "home-catalog-removal-and-return": ["MAPPING", "CONTROL"],
    "home-two-serving-only-profile": ["ENROLL"], "home-two-serving-only-has-no-issuer-routes-or-keys": ["ENROLL"],
    "dynamic-global-home-approved-third-home": ["ENROLL"], "travel-races-relay-data-addresses": ["CARRIER"],
    "home-two-state-persists-across-issuer-to-serving-only-restart": ["ENROLL"], "same-tcp-flow-relay-failover": ["CARRIER"],
    "home-two-same-tcp-flow-relay-failover": ["CARRIER"], "same-home-target-connection": ["CARRIER"],
    "server-failure-does-not-break-established-direct-flow": ["CARRIER", "CONTROL"],
    "server-failure-preserves-both-home-flows": ["CARRIER", "CONTROL"], "server-has-no-business-data-listener": ["CONTROL"],
    "udp": ["UDP"], "embedded-spa": ["LOCALUI"], "tls13-only": ["WIRE"], "slow-frame-deadline": ["WIRE"],
    "duplicate-travel-login-rejected-across-relays": ["CONTROL"], "encrypted-remote-travel-enrollment": ["ENROLL"],
    "tampered-enrollment-trust-rejected": ["ENROLL", "TRUST"], "independent-enrollment-nonce-bound": ["ENROLL"],
    "single-file-enrollment-response-includes-ca-roots": ["ENROLL"], "home-embedded-issuer-ui": ["LOCALUI", "ENROLL"],
    "home-action-inbox-excludes-history": ["ENROLL", "LOCALUI"], "home-credential-history-pagination-filter-and-search": ["ENROLL", "LOCALUI"],
    "home-manual-issuance-route-removed": ["ENROLL"], "home-dual-ca-issuance": ["ENROLL"], "duplicate-enrollment-issue-is-idempotent": ["ENROLL"],
    "enrollment-request-is-single-use": ["ENROLL"], "issuance-ledger-survives-home-restart": ["ENROLL"],
    "home-private-key-password-rotation": ["ENROLL"], "travel-private-key-password-rotation": ["ENROLL"],
    "password-rotation-survives-restart": ["ENROLL"], "issuer-reports-unsynchronized-authorization": ["CONTROL", "LOCALUI"],
    "deployment-root-private-key-not-mounted-in-home": ["TRUST"],
    "durable-control-high-water-survives-restart": ["AUTHZ", "CONTROL"], "expired-snapshot-bootstrap-through-learned-relay": ["TRUST", "CONTROL"],
    "fresh-directory-required-before-learned-relay-business": ["CONTROL", "AUTHZ"], "default-one-year-validity": ["ENROLL"],
    "global-super-authorization": ["AUTHZ"], "home-scoped-authorization": ["AUTHZ"], "service-scoped-authorization": ["AUTHZ"],
    "catalog-filtered-by-signed-scope": ["AUTHZ", "MAPPING"], "cross-home-revocation-rejected": ["AUTHZ"],
    "home-originated-live-revocation": ["AUTHZ"], "duplicate-issuance-cannot-reactivate-revoked-grant": ["AUTHZ"],
    "live-revocation-closes-flows-on-both-homes": ["AUTHZ", "CARRIER"], "revoked-certificate-rejected-by-both-relays": ["AUTHZ"],
    "revocation-without-process-restart": ["AUTHZ"], "home-revocation-idempotent": ["AUTHZ"], "restarted-relay-retains-revocation": ["AUTHZ"],
    "five-minute-local-statistics": ["STATS"], "signed-statistics-upload": ["STATS"], "server-statistics-idempotent-collection": ["STATS"],
    "day-week-month-year-statistics-queries": ["STATS"], "statistics-write-failure-isolated-per-home": ["STATS"],
    "home-two-password-gated-revocation": ["AUTHZ", "ENROLL"],
}
source = (REPO / "tests/e2e/assert_e2e.py").read_text()
checks = ast.literal_eval("[" + re.search(r"\nchecks = \[(.*?)\n\]", source, re.S).group(1) + "]")
for check in checks:
    modules = E2E.get(check) or (["ENROLL"] if "remote-enrollment" in check else ["UNSORTED"])
    add(f"e2e:{check}", "L3", modules, "docker", "make e2e", "tests/e2e/assert_e2e.py")

for check, modules, location in [
    ("e2e-image-version", ["PACKAGING"], "tests/e2e/run.sh"),
    ("runtime-bootstrap-config-boundary", ["TRUST"], "tests/e2e/run.sh"),
    ("authorization-state-explicit-initialization", ["AUTHZ"], "tests/e2e/run.sh"),
    ("passive-and-active-relay-control", ["CONTROL"], "tests/e2e/run.sh"),
    ("complete-relay-directory-broadcast", ["CONTROL"], "tests/e2e/run.sh"),
    ("socket-api-existing-home-and-issuer-free-in-process-home", ["SDK"], "tests/e2e/check-socket-api.sh"),
    ("in-process-home-shutdown-store-reopened", ["SDK", "RUNTIME"], "tests/e2e/check-socket-api.sh"),
    ("serving-only-subset-provisioning", ["ENROLL", "SDK"], "tests/e2e/check-business.py"),
    ("directed-travel-and-issuer-ack", ["ENROLL"], "tests/e2e/check-business.py"),
    ("idempotent-install-resume", ["ENROLL"], "tests/e2e/check-business.py"),
    ("descriptor-config-binding", ["ENROLL", "AUTHZ"], "tests/e2e/check-business.py"),
    ("business-home-restart", ["SDK", "RUNTIME"], "tests/e2e/check-business.py"),
    ("class-secondary-serving-only-home", ["CLASS", "ENROLL"], "tests/e2e/check-service-class.py"),
    ("class-relocation-preserves-all-files", ["CLASS"], "tests/e2e/check-service-class.py"),
    ("class-relocation-binding-and-digest-enforcement", ["CLASS"], "tests/e2e/check-service-class.py"),
    ("class-relocation-runtime-multi-home", ["CLASS"], "tests/e2e/check-service-class.py"),
    ("class-relocation-identity-unchanged", ["CLASS"], "tests/e2e/check-service-class.py"),
    ("class-revocation-closes-all-homes-keep-serving", ["CLASS", "AUTHZ"], "tests/e2e/check-service-class.py"),
    ("class-expiry-closes-all-homes-keep-serving", ["CLASS", "AUTHZ"], "tests/e2e/check-service-class.py"),
]:
    add(f"e2e:{check}", "L3", modules, "docker", "make e2e", location)

# The selectable units. assert_e2e.py stages carry their modules in its STAGES table; run.sh groups
# run when FLOWSPLICE_E2E_MODULES names one of the modules in their `checks_for` line.
stage_tree = ast.parse((REPO / "tests/e2e/assert_e2e.py").read_text())
stage_table = next(node.value for node in stage_tree.body if isinstance(node, ast.Assign)
                   and getattr(node.targets[0], "id", "") == "STAGES")
for entry in stage_table.elts:
    name = entry.elts[0].value
    modules = sorted(element.value for element in entry.elts[1].elts)
    needs = " ".join(element.value for element in entry.elts[2].elts)
    add(f"e2e-stage:{name}", "L3", modules, "docker", "FLOWSPLICE_E2E_MODULES=<module> make e2e",
        "tests/e2e/assert_e2e.py", f"needs {needs}" if needs else "selectable stage")
RUN_SH_GROUPS = {
    "ENROLL CARRIER": "dynamic-homes-and-data-address-race",
    "ENROLL AUTHZ": "first-travel-bootstrap-and-revocation",
    "SDK RUNTIME": "socket-api",
    "ENROLL SDK CLASS AUTHZ": "business-and-service-class",
    "CONTROL": "log-events-control",
    "CARRIER": "log-events-carrier",
    "AUTHZ": "log-events-authorization",
}
found = sorted(set(re.findall(r"checks_for ([A-Z ]+?)(?:;| &&|\s*$)", (REPO / "tests/e2e/run.sh").read_text(), re.M)))
if set(found) != set(RUN_SH_GROUPS):
    raise SystemExit(f"run.sh groups changed: {found}; update RUN_SH_GROUPS")
for modules, group in RUN_SH_GROUPS.items():
    add(f"e2e-group:{group}", "L3", modules.split(), "docker", "FLOWSPLICE_E2E_MODULES=<module> make e2e",
        "tests/e2e/run.sh", "selectable group")

for event, modules in [
    ("relay_directory_updated", ["CONTROL"]), ("carrier_race_started", ["CARRIER"]), ("active_carrier_race_sent", ["CARRIER"]),
    ("carrier_race_ack", ["CARRIER"]), ("carrier_reevaluation_scheduled", ["CARRIER"]), ("flow_detached", ["CARRIER"]),
    ("carrier_recovery_started", ["CARRIER"]), ("tcp_retransmit", ["CARRIER"]),
    ("carrier-reevaluation-stable-not-switched", ["CARRIER"]),
    ("travel_credential_revoked_by_home", ["AUTHZ"]), ("travel_authorization_published", ["AUTHZ"]),
    ("travel_authorization_ack", ["AUTHZ"]), ("travel_authorization_applied", ["AUTHZ"]), ("revoked_flow_closed", ["AUTHZ", "CARRIER"]),
]:
    add(f"e2e-log:{event}", "L3", modules, "docker", "make e2e", "tests/e2e/run.sh", "required log event")

# --- Static, policy and packaging checks ---------------------------------------------------------
for name, level, modules, runner in [
    ("tests/check-docker-pull-policy.sh", "L2", ["PACKAGING"], "make check"),
    ("tests/check-home2-macos-package.sh", "L2", ["PACKAGING", "MACOS"], "make check"),
    ("tests/check-travel-macos-package.sh", "L2", ["PACKAGING", "MACOS"], "make check"),
    ("tests/check-apple-products.sh", "L2", ["PACKAGING", "IOS", "MACOS"], "make check"),
    ("tests/check-release-feature-gates.sh", "L2", ["PACKAGING"], "manual"),
    ("tests/check-runtime-configuration-boundary.sh", "L2", ["TRUST"], "manual"),
    ("tests/check-sdk-consumer.sh", "L2", ["SDK"], "make sdk-check"),
    ("tests/test_package_privacy.py", "L2", ["PACKAGING"], "make check"),
    ("tests/test_private_travel_trust.py", "L2", ["PACKAGING", "TRUST"], "make check"),
    ("tests/openwrt/check.sh", "L2", ["PACKAGING", "LOCALUI"], "make check"),
    ("make apple-products (signed, notarized)", "L5", ["PACKAGING", "IOS", "MACOS"], "make apple-products"),
    ("cross-version compatibility (old backend + new Travel)", "L5", ["CONTROL", "CARRIER", "AUTHZ"], "manual"),
]:
    add(f"policy:{name}", level, modules, "host", runner, name)

# --- Native platforms ---------------------------------------------------------------------------
# Functional module of native unit tests, by name (platform module is always added).
NATIVE_UNIT_RULES = [
    (r"(?i)deploymentRoot", ["TRUST"]),
    (r"(?i)relay|enroll|TravelIds|normalizedIDs|credential|reenrollment", ["ENROLL"]),
    (r"(?i)catalog", ["MAPPING"]),
    (r"(?i)traffic|status|notification", ["DIAG"]),
    (r"(?i)storage|installation|configPath|reconciliation", ["ENROLL", "RUNTIME"]),
    (r"(?i)audio|stop|start|lifecycle|liveActivity", []),
]
for path, platform, level, runner in [
    ("travel-android/app/src/test/java/io/zxf/flowsplice/travel/TravelUnitTest.kt", "android-jvm", "L1", "gradle test"),
    ("travel-apple/FlowSpliceTravel/FlowSpliceTravelTests/FlowSpliceTravelTests.swift", "ios-sim", "L1", "xcodebuild test"),
]:
    text = (REPO / path).read_text()
    names = re.findall(r"@Test\s+fun\s+`?([A-Za-z0-9_ ]+?)`?\(", text) + re.findall(r"@Test(?:\([^)]*\))?\s+(?:@MainActor\s+)?func\s+([A-Za-z0-9_]+)\(", text)
    module = "ANDROID" if "android" in platform else "IOS"
    for name in names:
        functional = next((mods for pattern, mods in NATIVE_UNIT_RULES if re.search(pattern, name)), [])
        add(f"native:{name}", level, [module] + functional, platform, runner, path)

NATIVE = [
    # (id, level, modules, platform, location, note)
    ("android:catalogChoicesDetermineHomeServiceAndProtocol", "L4", ["ANDROID", "MAPPING"], "android-emu", "MappingDialogTest.kt", "no backend"),
    ("android:notificationUsesSystemStatusIcon", "L4", ["ANDROID"], "android-emu", "NotificationContractTest.kt", "no backend"),
    ("android:appRequestsPermissionForBoundedScreenOffGracePeriod", "L4", ["ANDROID"], "android-emu", "NotificationContractTest.kt", "no backend"),
    ("android:screenOffWakeLockExpiresAndCanBeReleasedEarly", "L4", ["ANDROID"], "android-emu", "NotificationContractTest.kt", "no backend"),
    ("android:productionScreenOffGraceExpiresAfterThreeIdleMinutes", "L5", ["ANDROID"], "android-emu", "NotificationContractTest.kt", "waits 3 min"),
    ("android:launchDisplaysNativeTravelDashboard", "L4", ["ANDROID"], "android-emu", "TravelAppE2ETest.kt", "no backend"),
    ("android:homeIdKeepsTrailingSeparatorWhileTyping", "L4", ["ANDROID", "ENROLL"], "android-emu", "TravelAppE2ETest.kt", "no backend"),
    ("android:passwordsSurviveActivityRecreation", "L4", ["ANDROID", "ENROLL"], "android-emu", "TravelAppE2ETest.kt", "no backend"),
    ("android-docker:enrollment-and-foreground-roundtrip", "L4", ["ANDROID", "ENROLL", "CARRIER"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("android-docker:boot-receiver-not-exported", "L4", ["ANDROID"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("android-docker:idle-cpu-memory-threads-writes-grace-lock", "L4", ["ANDROID", "RUNTIME"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", "performance sample"),
    ("android-docker:background-roundtrip", "L4", ["ANDROID", "CARRIER"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("android-docker:persistent-flow-across-wifi-cellular-switch", "L4", ["ANDROID", "CARRIER"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("android-docker:persistent-flow-across-short-outage", "L4", ["ANDROID", "CARRIER"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("android-docker:persistent-flow-after-doze", "L4", ["ANDROID", "CARRIER"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", "20 s forced idle"),
    ("android-docker:restart-cycles-keep-mapping", "L4", ["ANDROID", "MAPPING"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("android-docker:rapid-restart", "L4", ["ANDROID", "MAPPING"], "android-emu+docker", "TravelCoreDockerE2ETest.kt", ""),
    ("ios:testEnrollmentUsesNativeAdaptiveForm", "L4", ["IOS", "ENROLL"], "ios-sim", "FlowSpliceTravelUITests.swift", "no backend"),
    ("ios:testEnrollmentValidationEnablesSubmission", "L4", ["IOS", "ENROLL"], "ios-sim", "FlowSpliceTravelUITests.swift", "no backend"),
    ("ios:launch", "L4", ["IOS"], "ios-sim", "FlowSpliceTravelUITestsLaunchTests.swift", "no backend"),
    ("ios-docker:enrollment-catalog-mapping", "L4", ["IOS", "ENROLL", "MAPPING"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:foreground-roundtrip", "L4", ["IOS", "CARRIER"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:background-audio-and-live-activity", "L4", ["IOS"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:background-continuous", "L4", ["IOS", "CARRIER"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", "120 s by default"),
    ("ios-docker:sustained-background", "L5", ["IOS", "CARRIER"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", "900 s by default"),
    ("ios-docker:background-recovery", "L4", ["IOS", "CARRIER"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:after-cold-launch-restart", "L4", ["IOS", "MAPPING"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:relay-outage-while-backgrounded", "L4", ["IOS", "CARRIER", "CONTROL"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", "flaky under load (SRC-080)"),
    ("ios-docker:after-network-change", "L4", ["IOS", "CARRIER"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:after-runtime-restart", "L4", ["IOS", "MAPPING"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("ios-docker:while-and-after-screen-off", "L4", ["IOS", "CARRIER"], "ios-sim+docker", "FlowSpliceTravelUITests.swift", ""),
    ("macos:testEnrollmentValidationAndCompletion", "L4", ["MACOS", "ENROLL"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testControlCenterMappingsDiagnosticsAndLifecycle", "L4", ["MACOS", "MAPPING", "DIAG"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testMenuBarIconUsesNativeStatusItemFootprint", "L4", ["MACOS"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testCloseButtonHidesButDoesNotTerminate", "L4", ["MACOS"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testCommandQHidesButDoesNotTerminate", "L4", ["MACOS"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testDockQuitHidesButDoesNotTerminate", "L4", ["MACOS"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testStatusBarRealQuitShowsCancelAndConfirm", "L4", ["MACOS"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:testStatusBarRealQuitConfirmsWithNoActiveFlows", "L4", ["MACOS"], "macos", "FlowSpliceMacUITests.swift", "local suite"),
    ("macos:launch", "L4", ["MACOS"], "macos", "FlowSpliceMacUITestsLaunchTests.swift", "local suite"),
    ("macos-docker:foreground", "L4", ["MACOS", "ENROLL", "CARRIER"], "macos+docker", "FlowSpliceMacUITests.swift", ""),
    ("macos-docker:persistent-flow-across-relay-outage", "L4", ["MACOS", "CARRIER"], "macos+docker", "FlowSpliceMacUITests.swift", ""),
    ("macos-docker:new-flow-after-outage", "L4", ["MACOS", "CARRIER"], "macos+docker", "FlowSpliceMacUITests.swift", ""),
    ("macos-docker:menu-bar-only-keeps-running", "L4", ["MACOS"], "macos+docker", "FlowSpliceMacUITests.swift", ""),
    ("macos-docker:after-runtime-restart", "L4", ["MACOS", "MAPPING"], "macos+docker", "FlowSpliceMacUITests.swift", ""),
    ("macos-docker:after-cold-launch", "L4", ["MACOS", "MAPPING"], "macos+docker", "FlowSpliceMacUITests.swift", ""),
    ("phone:magic-v5-acceptance", "L5", ["ANDROID", "CARRIER", "ENROLL"], "android-phone", "manual", "physical device"),
]
for test_id, level, modules, platform, location, note in NATIVE:
    runner = {"android": "android/run.sh", "ios": "apple/run.sh", "macos": "macos/run.sh", "phone": "manual"}[test_id.split(":")[0].split("-")[0]]
    add(f"native:{test_id}", level, modules, platform, runner, location, note)

unsorted = [r["id"] for r in rows if "UNSORTED" in r["modules"]]
if unsorted:
    raise SystemExit(f"untagged test points (add a rule): {unsorted}")
out = REPO / "tests/test-map.csv"
buffer = io.StringIO()
writer = csv.DictWriter(buffer, fieldnames=list(rows[0]), lineterminator="\n")
writer.writeheader()
writer.writerows(rows)
if CHECK:
    if not out.is_file() or out.read_text() != buffer.getvalue():
        raise SystemExit("tests/test-map.csv is out of date; run python3 tests/test-map.py")
    print(f"tests/test-map.csv is current ({len(rows)} test points)")
else:
    out.write_text(buffer.getvalue())
    print(f"{len(rows)} test points -> {out}")
