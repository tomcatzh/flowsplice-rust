# Private Travel packaging

Private Travel releases use a deployment-root public key that stays outside the
repository. The package helper validates a strict external TOML profile, freezes
that key once, and builds from a selected committed Git tree. It never creates a
root key or accepts a private key.

Use Python 3.11 or newer for profile parsing and the build entry point. The
Xcode resource-copy and path-check commands also support Apple's Python 3.9.

Keep the profile, deployment-root public key, Android keystore, release output
folder, and Apple credentials outside every Git worktree and its metadata. This
also applies to ignored files and symlink targets. Do not commit profile files,
trust roots, signing identities, password values, or release artifacts.

A profile has exactly these sections and fields. All path values are absolute
external paths; password fields name inherited environment variables rather
than containing passwords.

```toml
[trust]
deployment_root_public_key = "/absolute/external/deployment-root.pub"

[output]
directory = "/absolute/external/private-releases"

[android]
keystore_file = "/absolute/external/android-release.keystore"
key_alias = "release-alias"
store_password_env = "PRIVATE_TRAVEL_STORE_PASSWORD"
key_password_env = "PRIVATE_TRAVEL_KEY_PASSWORD"

[apple]
team_id = "APPLE_TEAM_ID"
notary_profile = "notary-keychain-profile"
ios_device = "paired-device-selector"
```

Store an alias profile at
`~/.config/flowsplice/private-packaging/profiles/<alias>.toml`, then run one of
these commands:

```sh
scripts/build-private-travel-packages.sh --profile <alias> --check-only
scripts/build-private-travel-packages.sh --profile-file /absolute/external/profile.toml --ref <committed-ref>
scripts/build-private-travel-packages.sh --profile <alias>
```

`--check-only` reads and validates the profile, root, keystore path, password
environment presence, selected committed source, Git privacy policy, and required
tools. It does not create staging files, build, sign, notarize, install, or
launch an app. It reports those actions as pending.

The helper also has focused commands for the Apple build scripts:

```sh
python3 scripts/private-travel-trust.py check-profile --profile-file /absolute/external/profile.toml
python3 scripts/private-travel-trust.py check-path --path /absolute/external/path
python3 scripts/private-travel-trust.py copy-root --source /absolute/external/deployment-root.pub --destination /absolute/external/frozen-root.pub
python3 scripts/private-travel-trust.py verify-root --source /absolute/external/frozen-root.pub --artifact-resource /absolute/external/deployment-root.pub
```

`check-path` accepts a nonexistent destination but requires its canonical path
to be external. `copy-root` validates a bounded uncompressed P-256 public point
and atomically writes the already-read snapshot. `verify-root` compares the
exact frozen bytes without displaying them.

A real build uses `git archive` for the selected commit and creates a mode 0700
stage on the configured output filesystem. It refuses a source index or commit
that contains the root text or tracked key, certificate, bootstrap-root, or
local instruction material. Android runs Gradle with `--no-daemon`,
`--no-build-cache`, and `--no-configuration-cache`; release password values are
only inherited through the configured environment names. The produced APK must
contain the frozen `assets/bootstrap/deployment-root.pub` and have the same
certificate fingerprint as the configured keystore.

The Apple builder receives the frozen snapshot through
`FLOWSPLICE_PRIVATE_TRUST_FILE`. It verifies the root in the exported IPA and
mounted DMG app resource, and verifies the configured signing team before the
helper accepts its nine Apple publication files. The helper accepts only those
nine files plus the verified Android APK. It rebuilds `SHA256SUMS` after that
merge and verifies one digest for every published file other than the manifest
itself, then atomically publishes `<version>-<commit12>` under the external
output directory. Logs, Gradle cache, Cargo target files, Xcode intermediates,
and temporary files remain in the owned stage and are removed after completion.

There is no trust-on-first-use fallback. When using `flowsplice-cli --relay`,
provide `--deployment-root-public-key /absolute/external/deployment-root.pub`
or an existing trusted `--bootstrap-config`; relay mode no longer learns a
root automatically.
