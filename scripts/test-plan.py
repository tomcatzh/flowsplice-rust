#!/usr/bin/env python3
"""Print the tests a change needs: levels L1-L5 by modules and platforms (docs/TESTING.md).

  scripts/test-plan.py [BASE]      changes from BASE (default HEAD~1) to the working tree
  scripts/test-plan.py --commit REV  the changes of one commit
  scripts/test-plan.py --release   the release gate (L5), whatever changed
  scripts/test-plan.py --check     test-map and module names are consistent

The first matching rule decides each changed path. A rule names the backend modules whose E2E
checks (L3) the change needs, the native platforms (L4) and extra L1/L2 gates. "all" means every
module; a change to the E2E harness itself needs the complete parallel run.
"""
import argparse
import csv
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
MODULES = ["TRUST", "WIRE", "ENROLL", "AUTHZ", "CONTROL", "CARRIER", "UDP", "MAPPING", "CLASS", "STATS", "SDK",
           "RUNTIME", "LOCALUI", "DIAG", "PACKAGING", "TOOLS", "ANDROID", "IOS", "MACOS"]
PLATFORMS = ["android", "iphone", "ipad", "macos"]
# Clients share the Rust Travel core; a core change is exercised on one Android and one iOS client.
CLIENTS = ["android", "iphone"]
ALL = ["all"]

# (path pattern, L3 modules, L4 platforms, extra gates); extra gates: "skip" (no tests),
# "harness" (complete parallel run), "review-regressions", "sdk-check".
RULES = [
    (r"^(docs/|design-demos/|README\.md$|LICENSE$|\.gitignore$)|\.md$", [], [], {"skip"}),
    (r"^tests/test-map\.(py|csv)$|^scripts/test-plan\.py$", [], [], set()),
    (r"^tests/e2e/android/", [], ["android"], set()),
    (r"^tests/e2e/apple/", [], ["iphone", "ipad"], set()),
    (r"^tests/e2e/macos/", [], ["macos"], set()),
    (r"^tests/e2e/|^docker/|^\.dockerignore$", ALL, PLATFORMS, {"harness"}),
    (r"^tests/fixtures/", ["SDK", "CLASS", "CARRIER"], [], set()),
    (r"^tests/check-travel-review-regressions\.sh$", [], [], {"review-regressions"}),
    (r"^tests/check-sdk-consumer\.sh$|^examples/", [], [], {"sdk-check"}),
    (r"^tests/|^scripts/|^openwrt/|^relay/src/openwrt\.rs$", [], [], set()),
    (r"^travel-android/rust/", ["SDK"], ["android"], set()),
    (r"^travel-android/", [], ["android"], set()),
    (r"^travel-apple/rust/", ["SDK"], ["iphone", "ipad", "macos"], set()),
    (r"^travel-apple/", [], ["iphone", "ipad"], set()),
    (r"^travel-macos/", [], ["macos"], set()),
    (r"^assets/", [], PLATFORMS, set()),
    (r"^internal/flowsplice-core/src/(frame|tls)\.rs$", ["WIRE"], CLIENTS, set()),
    (r"^internal/flowsplice-core/src/(deployment|config)\.rs$", ["TRUST"], CLIENTS, set()),
    (r"^internal/flowsplice-core/src/(carrier|route)\.rs$", ["CARRIER"], CLIENTS, set()),
    (r"^internal/flowsplice-core/src/authorization\.rs$", ["AUTHZ"], CLIENTS, set()),
    (r"^internal/flowsplice-core/src/statistics\.rs$", ["STATS"], [], set()),
    (r"^internal/flowsplice-core/tests/service_class\.rs$", ["CLASS"], [], set()),
    (r"^internal/flowsplice-core/", ALL, CLIENTS, set()),
    (r"^internal/flowsplice-enrollment/", ["ENROLL", "TRUST", "CLASS"], CLIENTS, set()),
    (r"^internal/flowsplice-transport/", ["RUNTIME", "SDK", "MAPPING"], CLIENTS, set()),
    (r"^internal/flowsplice-storage/", ["RUNTIME", "STATS"], [], set()),
    (r"^crates/flowsplice-travel-core/src/(tcp_flow|carrier_regressions)\.rs$", ["CARRIER"], CLIENTS, {"review-regressions"}),
    (r"^crates/flowsplice-travel-core/src/udp", ["UDP"], CLIENTS, set()),
    (r"^crates/flowsplice-travel-core/src/socket\.rs$", ["SDK"], [], {"sdk-check"}),
    (r"^crates/flowsplice-travel-core/src/service_class", ["CLASS"], [], set()),
    (r"^crates/flowsplice-travel-core/src/installation_paths\.rs$", ["ENROLL", "SDK"], CLIENTS, set()),
    (r"^crates/flowsplice-travel-core/src/review_regressions\.rs$", [], [], {"review-regressions"}),
    (r"^crates/flowsplice-travel-core/", ALL, CLIENTS, {"review-regressions", "sdk-check"}),
    (r"^crates/flowsplice-home-core/", ["RUNTIME", "CARRIER", "SDK"], [], {"sdk-check"}),
    (r"^server/", ["CONTROL", "AUTHZ", "STATS", "ENROLL", "MAPPING", "CLASS"], [], set()),
    (r"^relay/", ["CONTROL", "CARRIER"], CLIENTS, set()),
    (r"^homeagent/web/|^travelagent/web/", ["LOCALUI"], [], set()),
    (r"^homeagent/", ["ENROLL", "LOCALUI", "STATS", "MAPPING"], [], set()),
    (r"^travelagent/", ["MAPPING", "LOCALUI", "ENROLL"], [], set()),
    (r"^foobar/|^tools/", [], [], set()),
    (r"^Makefile$|^\.github/", [], [], set()),
    (r"^Cargo\.(toml|lock)$|^rust-toolchain\.toml$", ALL, CLIENTS, {"review-regressions", "sdk-check"}),
    (r".", ALL, CLIENTS, set()),
]


def changed_paths(base, commit=None):
    if commit:
        return subprocess.run(["git", "-C", str(REPO), "diff", "--name-only", f"{commit}~1", commit],
                              capture_output=True, text=True, check=True).stdout.split()
    committed = subprocess.run(["git", "-C", str(REPO), "diff", "--name-only", base, "HEAD"],
                               capture_output=True, text=True, check=True).stdout.split()
    working = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"],
                             capture_output=True, text=True, check=True).stdout.splitlines()
    return sorted(set(committed) | {line[3:].split(" -> ")[-1] for line in working})


def plan(paths):
    modules, platforms, extras, reasons = set(), set(), set(), []
    for path in paths:
        pattern, rule_modules, rule_platforms, rule_extras = next(rule for rule in RULES if re.search(rule[0], path))
        reasons.append((path, rule_modules, rule_platforms, sorted(rule_extras)))
        if "skip" in rule_extras:
            continue
        modules |= set(rule_modules)
        platforms |= set(rule_platforms)
        extras |= rule_extras | {"tests"}
    if "all" in modules:
        modules = {"all"}
    return modules, platforms, extras, reasons


def coverage(levels, modules, platforms):
    rows = list(csv.DictReader((REPO / "tests/test-map.csv").open()))
    picked = [row for row in rows if row["level"] in levels and (
        row["level"] in ("L1", "L2") or "all" in modules
        or (row["level"] == "L3" and set(row["modules"].split()) & modules)
        or (row["level"] == "L4" and any(row["id"].startswith(f"native:{p}") for p in
                                         [{"iphone": "ios", "ipad": "ios"}.get(p, p) for p in platforms])))]
    return len(picked)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("base", nargs="?", default="HEAD~1")
    parser.add_argument("--commit")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.check:
        subprocess.run([sys.executable, str(REPO / "tests/test-map.py"), "--check"], check=True)
        used = {m for _, mods, _, _ in RULES for m in mods if m != "all"}
        tagged = {m for row in csv.DictReader((REPO / "tests/test-map.csv").open()) for m in row["modules"].split()}
        unknown = (used | tagged) - set(MODULES)
        if unknown:
            raise SystemExit(f"modules not in MODULES: {sorted(unknown)}")
        print("test plan rules and test map are consistent")
        return
    out = "/tmp/flowsplice-acceptance"
    if args.release:
        print("L5 release gate:")
        print("  make check && make test && make sdk-check && make review-regressions")
        print(f"  python3 tests/e2e/run-parallel.py {out}-release        # all modules, 120 s/900 s Apple phases")
        print("  manual: physical Magic V5, make apple-products (signed, notarized), old backend with new Travel")
        return
    modules, platforms, extras, reasons = plan(changed_paths(args.base, args.commit))
    for path, rule_modules, rule_platforms, rule_extras in reasons:
        print(f"  {path}: L3 {' '.join(rule_modules) or '-'}; L4 {' '.join(rule_platforms) or '-'}"
              + (f"; {' '.join(rule_extras)}" if rule_extras else ""))
    if "tests" not in extras:
        print("No tests needed (documentation only).")
        return
    print("L1/L2:")
    print("  python3 tests/test-map.py --check && make check && make test")
    for gate in ("sdk-check", "review-regressions"):
        if gate in extras:
            print(f"  make {gate}")
    if "harness" in extras:
        print("Harness change, complete parallel run (L3 all, L4 all platforms):")
        print(f"  python3 tests/e2e/run-parallel.py {out} --apple-short")
        print(f"  covers {coverage({'L3', 'L4'}, {'all'}, set(PLATFORMS))} L3/L4 test points")
        return
    if modules:
        joined = " ".join(sorted(modules))
        print("L3:")
        print(f"  FLOWSPLICE_E2E_MODULES=\"{joined}\" make e2e")
        print(f"  covers {coverage({'L3'}, modules, set())} L3 test points")
    if platforms:
        listed = ",".join(p for p in PLATFORMS if p in platforms)
        print("L4 (native phases only, short Apple phases):")
        print(f"  python3 tests/e2e/run-parallel.py {out} --platforms {listed} --native-only --apple-short")
        print(f"  covers {coverage({'L4'}, set(), platforms)} L4 test points")


if __name__ == "__main__":
    main()
