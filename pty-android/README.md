# Private Android PTY host

The ordinary `./gradlew :app:assembleDebug` build contains no private configuration or native library and displays an unconfigured-installation message. The package ID is `io.zxf.flowsplice.pty`. It has only INTERNET permission and no background service.

Use `scripts/build-private.py --root /external/deployment-root.pub --service-class /external/service-class.json --output /external/new-build-directory --target x86_64-linux-android` for isolated emulator builds, or select `aarch64-linux-android` for ARM64. Set JAVA_HOME and ANDROID_SDK_ROOT to the established Android Studio JBR and SDK. The script exports current tracked/unignored sources, builds the local frontend and JNI library, and keeps all inputs, outputs, Cargo/Gradle/npm caches and the debug signing key outside Git. The output directory must not already exist.

The primary mode uses one class identity and one foreground Travel runtime for up to eight simultaneous PTY Home targets. A single Global issuer approval covers current and future properly authorized Homes whose class is exactly `flowsplice.pty.v1` / `tcp`. Targets are discovered from the service directory; each Home needs no separate enrollment or password. The `service-class.json` input is:

```json
{"version":1,"approving_home_id":"super-home","application_protocol":"flowsplice.pty.v1","protocol":"tcp"}
```

`--service-class`, `--homes` and `--descriptor` are mutually exclusive. The class installation uses `filesDir/service-class` and separate identity and encrypted-password preferences. Legacy `--homes /external/homes.json` and `--descriptor /external/business.json` packages remain supported; their profile directories and secure-store accounts are retained, including `default`. No existing credential is automatically widened. See [the PTY guide](../docs/pty.md).

Upgrade every Server, Relay and Home authorization consumer before issuing the first class grant. Older infrastructure cannot parse the new scope enum; existing generic Travel applications themselves remain unchanged.

For release, add `--release` and supply durable external signing through FLOWSPLICE_PTY_KEYSTORE, FLOWSPLICE_PTY_KEY_ALIAS, FLOWSPLICE_PTY_STORE_PASSWORD and FLOWSPLICE_PTY_KEY_PASSWORD in the environment. Never place deployment resources or signing keys in this source tree.

The WebView loads only bundled assets under `https://appassets.androidplatform.net/assets/pty/`; every other request is blocked. Polling runs at 25ms only while foreground and page-ready. Leaving the activity disconnects all Homes; returning never reconnects automatically. Enrollment passwords are saved before enrollment starts using Android Keystore AES-GCM protection. Normal connections reuse the stored password without a login prompt.

The PTY family uses the shared [terminal ribbon icon](../assets/brand/pty/README.md). Generated platform assets are checked in; regenerate them with `python3 scripts/export-pty-icons.py` after an artwork update.
