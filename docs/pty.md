# Private FlowSplice PTY

The first PTY application uses the shared encrypted Home/Travel crates. Its Travel
side opens no local TCP, UDP, HTTP or WebSocket listener. Each connected Home owns
an independent foreground runtime. Switching Home pages or terminal tabs preserves
other connections; disconnecting one Home releases only that runtime. Backgrounding
the app disconnects all Homes.
The generic Travel applications retain their existing forwarding and background behavior.

## Provisioning

Use the generic Home setup command to request a serving-only business Home. The
service request file is an array such as:

```json
[{"service_id":"terminal","protocol":"tcp","application_protocol":"flowsplice.pty.v1","capabilities":["read","write"]}]
```

Pass the private file to `flowsplice-homeagent init --business-services FILE --install-dir DIR`
with the existing bootstrap/server arguments. The Super Home approves the exact
service set (or a subset). The resulting `home-runtime.toml`, serving credentials,
service grant and individual `business-*.json` descriptors are private installation
material. The PTY runtime does not need CA signing keys, an issuer password or an
administration UI.

Configure one private tmux domain for every approved service:

```toml
home_runtime = "/srv/flowsplice-pty/home-runtime.toml"

[[domains]]
service_id = "terminal"
[domains.tmux]
binary = "/usr/bin/tmux"
socket = "/srv/flowsplice-pty/domain/tmux.sock"
shell = "/bin/bash"
working_directory = "/srv/terminal-work"
```

Run `flowsplice-pty-home --config FILE` under the account that should own the shells.
The configured shell and tmux executable must exist. The socket's parent directory
must belong to that account and have mode 0700; one Home process holds the domain
lock. Use separate accounts and domains when stronger operating-system isolation is
needed. First-version backends target Linux arm64/amd64 and macOS arm64, with tmux
3.3 or later; the acceptance suite exercises actual installed tmux versions.

Package the deployment root and the chosen business descriptors as private resources
using [Android packaging](../pty-android/README.md) or [Apple packaging](../pty-apple/README.md).
Neither private configuration nor keys belong in Git or neutral Rust binaries. The
client initially asks for the Relay IP:port and private-key password, waits for the fixed
Super Home's approval, and stores the approved service binding automatically. Its
password is saved in native secure storage before enrollment starts so interrupted
enrollment can resume. Daily connections use that stored password without a login
prompt. A missing stored credential opens a recovery prompt.

Private multi-Home builds accept `homes.json` in place of `business.json`:

```json
{"version":1,"homes":[{"id":"default","name":"Mac 工作站","platform":"macos","relay":"192.0.2.1:8443","descriptor":{}}]}
```

Replace the illustrative empty descriptor with that Home's complete signed business
descriptor. A catalog contains one to eight unique profile IDs, matching
`[a-z0-9][a-z0-9_-]{0,47}`, and each profile selects `linux` or `macos`. Each profile
has isolated enrollment state and secure credentials. Keep the existing Home under
`default`: that profile preserves the previous single-Home installation directory,
Travel ID and secure-store account. A legacy package containing only `business.json`
still loads that default profile.

## Sessions and ownership

A successful connection opens the session list. Creation first prompts for a display
name, then creates one tmux session with one shell and joins it read-write. Names are
trimmed, may repeat, and accept up to 64 Unicode scalars / 256 UTF-8 bytes without
control characters. The internal UUID stays separate from the display name.

The list shows creation time, latest successful attachment time and active attachment
count. Read-only observers count; clients only browsing the list do not. Names and
latest attachment times are stored in private tmux options and survive Home restart
with the session. Old unnamed sessions receive a short UUID-based display name.
Explicit `list_details` and `new_named` operations provide these additions while
the legacy List, New, Session and Attached shapes remain unchanged.

Join can request read-only or read-write; an occupied
session initially admits another client as an observer. Taking over requires a warning
and confirmation. Home checks the current ownership epoch for every input write,
and immediately demotes the previous writer without waiting for its acknowledgement.
Only the writer changes the shared terminal dimensions.

macOS uses the physical keyboard and has no clickable virtual terminal keys.
Android and iOS retain their on-screen terminal keys.

Closing a tab detaches its client. Disconnecting, application backgrounding, Home
process shutdown or loss of authorization does not intentionally terminate the shell.
Reconnection returns to the list, and an explicit join obtains a fresh screen. No input
or uncertain New request is automatically replayed. Exit the outermost shell normally
to end the tmux session; there is no remove/kill-session application command.
Orderly disconnect sends application EOF before shutting down the recoverable
transport, allowing Home to release the old writer. If the network is already
unavailable, a later client can use the same warned takeover mechanism.
An unexpected Carrier closure retains the configured transport recovery window
(90 seconds by default). Revocation or expiry ends Home's authorization immediately;
the client may finish reporting the disconnect after that recovery window. Neither
recovery attempts nor a later connection restore an expired or revoked grant.

## Service-manager lifetime

Keep the tmux daemon's lifetime separate from Home. A Linux systemd unit's default
`KillMode=control-group` also kills descendants when the unit stops. A dedicated Home
unit can use `KillMode=process` with no child-killing `ExecStop` to leave tmux running
across Home restarts. This deliberately permits descendants to outlive the unit; normal
machine shutdown, shell exit and tmux failure still end them. See the
[systemd kill semantics](https://www.freedesktop.org/software/systemd/man/latest/systemd.kill.html).

On macOS, launchd's `AbandonProcessGroup` controls cleanup of processes in the job's
process group. Ordinary tmux daemonization normally creates a separate session/process
group. Validate the installed launchd job and tmux build with an actual Home restart;
do not infer survival from a configuration flag alone. Logging out or restarting the
machine is outside the keepalive promise.

## Validation

`tests/e2e/check-pty-platforms.sh` runs real tmux ownership, blocked-input takeover,
resize, detach and natural-exit tests on both Linux architectures. Set
`FLOWSPLICE_PTY_E2E=1` for the full Docker runner's encrypted PTY scenario, including
Home process termination and crash/restart. Native UI tests live in `pty-android` and
`pty-apple`; external build drivers are under `tests/e2e/pty-native`.

The native drivers consume an externally exported disposable fixture and approve
only the exact request whose verification code was rendered in the app. Build
outputs and fixture secrets stay outside Git. On macOS, an existing developer
signing identity can be supplied to the test build through
`FLOWSPLICE_PTY_MACOS_SIGNING_IDENTITY` and, when needed,
`FLOWSPLICE_PTY_MACOS_DEVELOPMENT_TEAM`; signature verification and successful
test-runner launch are separate checks.

Use dedicated simulators with an English keyboard for deterministic shell-command
input. Apple UI acceptance reads the rendered terminal with local Vision OCR; the
submitted command encodes its expected output, so echoed input cannot satisfy the
check. Do not enable xterm's screen-reader mode just to expose text to a test:
that mode changes its software-keyboard input handling. Input-method composition
and physical-device acceptance require their own explicit evidence.

History, audit, OAuth Web verification and durable recording are deferred. Terminal
rendering and attachment recovery do not constitute an audit log.
