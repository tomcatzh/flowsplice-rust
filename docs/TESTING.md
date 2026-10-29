# Testing

Every test point is tagged in `tests/test-map.csv` with a **level** (how heavy it is) and one or
more **modules** (what it tests). A change runs only the levels and modules it touches;
`scripts/test-plan.py` prints them, with the exact commands.

```sh
scripts/test-plan.py                # changes since HEAD~1, including the working tree
scripts/test-plan.py --commit REV   # one commit
scripts/test-plan.py --release      # the release gate
scripts/test-plan.py --check        # test map and rules are consistent
python3 tests/test-map.py           # regenerate the map after adding or renaming tests
```

## Levels

| Level | What | Typical cost |
| --- | --- | --- |
| L1 unit | In-process tests: `cargo test`, Android JVM tests, Swift unit tests | seconds to minutes |
| L2 integration | Several components without Docker: regression suites with controlled time, policy and packaging checks | minutes |
| L3 system | The Docker topology: `make e2e` | about 15 minutes for all modules |
| L4 native | The Travel apps on the Android emulator, iOS simulators and macOS, against the Docker topology | 5–20 minutes per platform |
| L5 release | Long background phases (120 s and 900 s), the physical phone, signed products, an old backend with the new Travel | release only |

## Modules

| Module | Covers |
| --- | --- |
| TRUST | Deployment root, bootstrap configuration and discovery, pins |
| WIRE | Frames, write and read deadlines, TLS policy |
| ENROLL | Home installation and profiles, Travel enrollment, the issuer, issuance ledger, password rotation |
| AUTHZ | Authorization snapshots and scopes, revocation, rollback protection |
| CONTROL | Server and Relay control, the Relay directory, Travel login sessions, control backpressure |
| CARRIER | Carrier races, data-address races, Relay failover, flow recovery, TCP flows |
| UDP | UDP associations |
| MAPPING | Travel mappings and listeners, the catalog, Home and service selection |
| CLASS | Service classes |
| STATS | Statistics |
| SDK | The in-process SDK and socket API |
| RUNTIME | Tasks, transport, admission, storage |
| LOCALUI | Local Travel and Home issuer UIs |
| DIAG | Diagnostics |
| PACKAGING | Packaging, release and policy checks, OpenWrt |
| TOOLS | Probes and fixtures |
| ANDROID, IOS, MACOS | The platform shells |

## Running a selection

- **L3 by module:** `FLOWSPLICE_E2E_MODULES="CARRIER STATS" make e2e`.
  - The topology, the trust inputs and their quick checks always run.
  - The `run.sh` groups and the `assert_e2e.py` stages run when one of their modules is named.
  - A stage also runs every stage it needs (the `STAGES` table in `tests/e2e/assert_e2e.py`).
  - `all` is the default.
- **L4 without the L3 checks:** `python3 tests/e2e/run-parallel.py OUT --platforms android,macos --native-only`.
  - It starts each platform's topology and goes straight to the native phase.
  - Add `--apple-short` for 30 s and 60 s background phases on iPhone and iPad.
- **Complete:** `python3 tests/e2e/run-parallel.py OUT` runs all modules and all platforms in parallel, one topology each.

## Rules

- Only one macOS UI test may run at a time on this Mac. Run FlowSplice and Vaultty acceptance one after the other; both also use the same Android emulator.
- The Android Travel E2E clears the Travel app, so it runs only on an emulator.
- Before a macOS run, make sure no system dialog (for example "… quit unexpectedly") covers the screen.
- When a test is added or renamed, regenerate `tests/test-map.csv`; `--check` fails until it is current.
