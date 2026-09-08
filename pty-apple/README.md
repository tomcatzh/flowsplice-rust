# FlowSplice PTY Apple hosts

`FlowSplicePTY.xcodeproj` is maintained directly. Shared schemes are `FlowSplicePTY-iOS` and `FlowSplicePTY-macOS`; both compile the same Swift host and Rust `flowsplice-pty-native` ABI. UI comes exclusively from `../pty-web/dist` (build it with `npm ci && npm run build` in `pty-web`). Transport connections run inside the app; there is no localhost server.

Neutral builds omit `FLOWSPLICE_PTY_BOOTSTRAP_DIR` and display a private configuration missing message. Set `CARGO_TARGET_DIR` to choose the Rust build cache. The build phase selects Apple Silicon macOS, iOS simulator, or iOS device from the SDK.

Private packaging must use an externally staged, Git-free source export, derived data, build products, Cargo target directory and bootstrap input directory. Set `FLOWSPLICE_PTY_BOOTSTRAP_DIR` to an external directory containing `deployment-root.pub` and `service-class.json`. The resource script applies the repository's existing `private-travel-trust.py` external-path checks before copying. It rejects private builds directly from a Git checkout. Signing identity, team, provisioning and archive/export settings are supplied externally by the owner; none are embedded here. Never place private inputs in this directory.

The primary mode uses one service-class identity and one foreground Travel runtime for up to eight simultaneous PTY Home targets. One Global issuer approval covers current and future properly authorized Homes with the exact `flowsplice.pty.v1` / `tcp` service class. The app discovers matching Homes after connecting; there is no enrollment or password prompt for each Home. A private `service-class.json` contains:

```json
{"version":1,"approving_home_id":"super-home","application_protocol":"flowsplice.pty.v1","protocol":"tcp"}
```

The class installation lives in the `service-class` Application Support subdirectory with a separate identity and Keychain account. `service-class.json` is mutually exclusive with legacy `homes.json` and `business.json` bootstrap inputs. Legacy packages remain supported as described in [the PTY guide](../docs/pty.md): each legacy profile retains its existing installation and account, including `default`. Existing credentials are never automatically widened into a class grant.

Before issuing the first class grant, upgrade all Server, Relay and Home authorization consumers: older infrastructure cannot parse the new scope enum. Existing generic Travel applications remain unchanged.

Enrollment passwords are securely saved before enrollment starts; normal connections reuse them without a password prompt. Stored secrets are filled into Rust actions and never returned to JavaScript. Transient iOS inactivity preserves transport connections. Actual backgrounding pauses UI retries and begins a finite, at-most-30-second UIKit background grace (OS expiration may end it sooner); returning during grace preserves attachments. Expiry disconnects transports without deleting remote sessions or workspace metadata. Native polling continues independently of WebKit callbacks, with a bounded ordered pending queue; overflow discards the discontinuous stream and disconnects before restoration. macOS hiding, minimization and occlusion do not disconnect transports; closing the window may suspend them.

The host atomically saves strictly validated metadata-only workspace snapshots (version 1, at most 64 KiB, 8 Homes and 64 tabs) in private Application Support files, separately for class and legacy modes and isolated test namespaces. No passwords or output are stored. Every new WebKit page receives workspace metadata before native state. Content-process termination reloads bundled UI and gracefully disconnects stale attachments using the existing native actors; restoration waits for actual disconnected snapshots, avoiding overlapping installation-store owners.

The PTY family uses the shared [terminal ribbon icon](../assets/brand/pty/README.md). Generated platform assets are checked in; regenerate them with `python3 scripts/export-pty-icons.py` after an artwork update.

Isolated macOS native test intermediates use Xcode ad hoc signing (`CODE_SIGNING_ALLOWED=YES`, `CODE_SIGN_IDENTITY=-`) with ordinary Debug entitlements. Every build gets fresh test-only app and runner identities, avoiding sandbox containers owned by older signing identities. The native build driver verifies both signatures, requires ad hoc signing without a certificate authority, and rejects unsigned bundles. Ad hoc signing is authorized for tests only; final macOS distribution packages must be notarized after complete testing.
