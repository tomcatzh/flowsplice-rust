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
  - It starts the platforms' topologies and goes straight to the native phases.
  - Add `--apple-short` for 30 s and 60 s background phases on iPhone and iPad.
- **Complete:** `python3 tests/e2e/run-parallel.py OUT` runs everything at once in three topologies:
  - `l3`: all modules, once;
  - `apple`: iPhone, iPad and macOS together, with their own Travel IDs and mapping ports, taking
    turns stopping a Relay (`tests/e2e/relay-outage.py`);
  - `android`: alone, because the emulator needs another Relay discovery address (`10.0.2.2`).
  - The apple topology starts first, so it gets an OrbStack bridge (see the rules below).

## Rules

- Only one macOS UI test may run at a time on this Mac. Run FlowSplice and Vaultty acceptance one after the other; both also use the same Android emulator.
- A native test that stops a Relay uses `tests/e2e/relay-outage.py`: one outage at a time per topology,
  and the Relay counts as back only when its listeners are ready and the Server has it in the directory.
- Native test builds (iOS, macOS, Android) are reused while nothing they are built from changes
  (`tests/e2e/native-build-cache.py`): program sources and the platform's own directory, toolchains
  and the bundled deployment root. Changes to tests, docs, the backend services or another platform
  do not rebuild them. The cache is `~/Library/Caches/FlowSpliceE2E` (`FLOWSPLICE_E2E_CACHE_DIR`) and
  keeps three builds per platform; `FLOWSPLICE_E2E_NATIVE_CACHE=0` always builds.
  - Because the apps bundle the test deployment root, `generate-certs.sh` keeps one root in that cache
    on a Mac; every other key and certificate is still new each run. Delete
    `~/Library/Caches/FlowSpliceE2E/deployment-root` for a new root.
- The Android Travel E2E clears the Travel app, so it runs only on an emulator.
- Before a macOS run, make sure no system dialog (for example "… quit unexpectedly") covers the screen.
- The first iOS simulator run of the background-audio phase on a Mac makes macOS ask whether
  `SimulatorTrampoline.xpc` (the simulator's audio bridge) may use the microphone. The app only plays
  audio; deny it. Unanswered, the prompt stalls the simulator's audio, so the app is killed, and it
  covers any macOS UI test running at the same time.
- The macOS UI test accepts FlowSplice's own "find devices on the local network" prompt; prompts of
  other apps are left alone.
- The iPhone and iPad tests reach relay2 at its container address, so OrbStack must bridge the
  topology's Docker network to macOS. It bridges at most 8 active networks, other running Compose
  projects included, and logs "too many active networks" in `~/.orbstack/log/vmgr.log` for the rest.
  `tests/e2e/apple/run.sh` waits for the route before the UI test (`FLOWSPLICE_APPLE_E2E_ROUTE_WAIT_SECONDS`,
  default 3600). The parallel runner starts the apple topology first and the others once macOS routes
  it; if other running projects hold all 8 slots, stop one.
- When a test is added or renamed, regenerate `tests/test-map.csv`; `--check` fails until it is current.
