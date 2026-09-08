# FlowSplice PTY Apple hosts

`FlowSplicePTY.xcodeproj` is maintained directly. Shared schemes are `FlowSplicePTY-iOS` and `FlowSplicePTY-macOS`; both compile the same Swift host and Rust `flowsplice-pty-native` ABI. UI comes exclusively from `../pty-web/dist` (build it with `npm ci && npm run build` in `pty-web`). No localhost server or background keepalive is used.

Neutral builds omit `FLOWSPLICE_PTY_BOOTSTRAP_DIR` and display a private configuration missing message. Set `CARGO_TARGET_DIR` to choose the Rust build cache. The build phase selects Apple Silicon macOS, iOS simulator, or iOS device from the SDK.

Private packaging must use an externally staged, Git-free source export, derived data, build products, Cargo target directory and bootstrap input directory. Set `FLOWSPLICE_PTY_BOOTSTRAP_DIR` to an external directory containing `deployment-root.pub` and `business.json`. The resource script applies the repository's existing `private-travel-trust.py` external-path checks before copying. It rejects private builds directly from a Git checkout. Signing identity, team, provisioning and archive/export settings are supplied externally by the owner; none are embedded here. Never place private inputs in this directory.

For multiple Homes, provide `homes.json` instead of `business.json`, using the catalog described in [the PTY guide](../docs/pty.md). Each profile owns a native connection, an Application Support installation and a Keychain account. The `default` profile preserves the previous installation and account. Enrollment passwords are securely saved before enrollment starts; normal connections reuse them without a password prompt. Stored secrets are filled into Rust actions and never returned to JavaScript. Backgrounding disconnects every Home and suspends polling; foregrounding resumes polling without reconnecting.

The PTY family uses the shared [terminal ribbon icon](../assets/brand/pty/README.md). Generated platform assets are checked in; regenerate them with `python3 scripts/export-pty-icons.py` after an artwork update.
