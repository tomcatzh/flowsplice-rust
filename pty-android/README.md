# Private Android PTY host

The ordinary `./gradlew :app:assembleDebug` build contains no private configuration or native library and displays an unconfigured-installation message. The package ID is `io.zxf.flowsplice.pty`. It has only INTERNET permission and no background service.

Use `scripts/build-private.py --root /external/deployment-root.pub --descriptor /external/business.json --output /external/new-build-directory --target x86_64-linux-android` for isolated emulator builds, or select `aarch64-linux-android` for ARM64. Set JAVA_HOME and ANDROID_SDK_ROOT to the established Android Studio JBR and SDK. The script exports current tracked/unignored sources, builds the local frontend and JNI library, and keeps all inputs, outputs, Cargo/Gradle/npm caches and the debug signing key outside Git. The output directory must not already exist.

For release, add `--release` and supply durable external signing through FLOWSPLICE_PTY_KEYSTORE, FLOWSPLICE_PTY_KEY_ALIAS, FLOWSPLICE_PTY_STORE_PASSWORD and FLOWSPLICE_PTY_KEY_PASSWORD in the environment. Never place deployment resources or signing keys in this source tree.

The WebView loads only bundled assets under `https://appassets.androidplatform.net/assets/pty/`; every other request is blocked. Polling runs at 25ms only while foreground and page-ready. Leaving the activity requests disconnect; returning never reconnects automatically. Enrollment passwords are persisted with a distinct Android Keystore AES-GCM key only after the installed event.
