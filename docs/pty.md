# Private FlowSplice PTY

The first PTY application uses the shared encrypted Home/Travel crates. Its Travel
side opens no local TCP, UDP, HTTP or WebSocket listener. The primary service-class
mode has one identity and one foreground Travel runtime shared by up to eight
simultaneously connected PTY Home targets. Switching Home pages or terminal tabs
preserves other connections; disconnecting one Home releases only its PTY connection.
Android and iOS preserve connections during a finite background grace period of up
to 30 seconds (iOS may expire it earlier). macOS hiding, occlusion and minimization
preserve connections. After suspension or application restart, saved connection
intent restores the selected Homes and original terminal tabs automatically.
The generic Travel applications retain their existing forwarding and background behavior.

## Terminal scrollback

Scroll upward with the mouse/trackpad, drag downward on a touch screen, or use
Shift–Page Up to browse tmux output. Older rows load on demand without moving the
current reading position. A floating down arrow appears while browsing; it returns
to the latest live terminal output. Shift–Page Down and Shift–End also navigate
back toward live output. Read-only clients can browse the same history.

Home configures tmux to retain up to 50,000 physical history rows per pane. This is
tmux's bounded in-memory history, not an audit log or permanent recording. Already
discarded output cannot be recovered. On tmux 3.7 and newer the limit also updates
existing panes; older supported tmux versions apply it to newly created panes.
Existing shells are preserved during an upgrade.

Each browsing pass captures a stable snapshot of retained history and the visible
screen. Pages contain at most 256 rows and stay within the application frame limit.
New output continues in the live terminal without pulling the history view to the
bottom. Loaded rows are cached in the current tab's memory; returning to history
reuses the cache when there has been no new output. A fresh browsing pass after new
output refreshes the snapshot. Reattachment, deletion and tab closure discard the
cache. Terminal bytes are never replayed as input or persisted to the workspace.
History capture never enters tmux copy mode, changes writer ownership or resizes
another client's terminal.

Captures are bounded to 64 MiB each and 256 MiB of retained cache across a Home
process, with one capture at a time. An unusually large history or exhausted cache
returns a retryable error without disconnecting the terminal. Text, Unicode and
SGR styling are rendered as inert history; links and terminal control actions are
not executed by the history viewer.

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

Package the deployment root and a `service-class.json` descriptor as private resources
using [Android packaging](../pty-android/README.md) or [Apple packaging](../pty-apple/README.md):

```json
{"version":1,"approving_home_id":"super-home","application_protocol":"flowsplice.pty.v1","protocol":"tcp"}
```

Neither private configuration nor keys belong in Git or neutral Rust binaries. The
client asks for the Relay IP:port and private-key password once, displays its device
name followed by ` · PTY`, and waits for the specified Global issuer's approval. This
approval covers current and future properly authorized business Homes with the exact
application protocol and transport in the descriptor. It is not Global Travel access,
and a matching alias or service ID does not confer authorization.

After approval, one class identity connects the shared runtime and discovers authorized
PTY targets from the service directory. Connecting each Home opens its session list;
there is no per-Home enrollment or password prompt. The class installation and native
secure-store namespace are separate from existing installations. The enrollment password
is saved securely before enrollment starts so interrupted enrollment can resume. Daily
connections reuse it without a login prompt; a missing credential opens recovery.

Upgrade all Server, Relay and Home authorization consumers before issuing the first
service-class grant. Older infrastructure cannot parse the new credential-scope enum.
Existing generic Travel applications remain unchanged; their credentials and forwarding
behavior are preserved. Existing narrow credentials are never automatically widened.

### Legacy private bootstrap compatibility

`service-class.json` is mutually exclusive with the legacy bootstrap files below.
These legacy packages continue to use separate per-Home identities and foreground
runtimes; their installed files and secure-store accounts remain intact.

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

Rename is available from the session list and terminal controls. It requires an
active business connection with write permission, independently of the terminal
writer lease. Saving changes the persistent display name without recreating the
shell or changing attachments, ownership, dimensions or timestamps. Cancel before
saving leaves the name unchanged. After Home confirms the `rename` request, the
client refreshes names in the list, tabs and terminal switcher; other connected
clients pick up the name through their existing five-second details refresh.

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
Reconnection refreshes each connected Home's session list and rejoins saved session
IDs with a fresh screen. Original read-only tabs stay read-only; original writers
request write access but fall back to read-only when another writer occupies the
session. Restoration never forces takeover, creates a shell or replays terminal input.
Explicitly closing a tab or disconnecting a Home removes its restoration intent.
An externally deleted tmux session remains as a disabled gray tab labeled “已删除”;
unavailable Homes remain pending until their authoritative session list is available.
Only bounded workspace metadata is persisted; passwords remain in native secure
storage and terminal input/output is never stored in the workspace file.
No uncertain New request is automatically replayed. Exit the outermost shell normally
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

Deploy a coordinated current-version system. Mixed old/new runtime support and old-client compatibility tests are not required release gates. Run configuration migration tests only when a configuration format changes; retain current-version functional, regression, E2E and Release validation. Preserve existing configuration and identity material during ordinary upgrades.

`tests/e2e/check-pty-platforms.sh` runs real tmux ownership, blocked-input takeover,
resize, detach and natural-exit tests on both Linux architectures. Set
`FLOWSPLICE_PTY_E2E=1` for the full Docker runner's encrypted PTY scenario, including
Home process termination and crash/restart. Native UI tests live in `pty-android` and
`pty-apple`; external build drivers are under `tests/e2e/pty-native`.

The native drivers consume an externally exported disposable fixture and approve
only the exact request whose verification code was rendered in the app. Class-mode
acceptance additionally matches the rendered device label and full descriptor, issues
exactly one class approval, and connects two distinct Homes through the shared runtime. Build
outputs and fixture secrets stay outside Git. Isolated macOS native tests use Xcode
ad hoc signing with `CODE_SIGNING_ALLOWED=YES` and `CODE_SIGN_IDENTITY=-`,
preserving ordinary Debug entitlements. Both app and runner must pass signature
verification and report ad hoc signing without a certificate authority; unsigned
bundles are rejected. Each build uses a fresh test-only application and runner
identity, avoiding sandbox containers owned by older signing identities. This
authorization is for tests only. Final macOS distribution
packages require Apple notarization after the complete test suite passes.

Use dedicated simulators with an English keyboard for deterministic shell-command
input. Apple UI acceptance reads the rendered terminal with local Vision OCR; the
submitted command encodes its expected output, so echoed input cannot satisfy the
check. Do not enable xterm's screen-reader mode just to expose text to a test:
that mode changes its software-keyboard input handling. Input-method composition
and physical-device acceptance require their own explicit evidence.

Audit, OAuth Web verification and durable history recording are deferred. The
in-memory terminal scrollback and attachment recovery do not constitute an audit log.
