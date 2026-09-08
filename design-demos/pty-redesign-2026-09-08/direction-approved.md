# Approved implementation direction — 2026-09-08

The owner selected **A: horizontal desktop tabs** and approved the revised phone workflow. The terminal uses the largest available area. macOS has no clickable virtual keys. Default connection reuses the existing securely stored private-key password and does not ask for a startup/login password.

Implementation is authorized for simultaneous multi-Home clients, the selected multi-page interface, naming before session creation, creation/latest-join timestamps and active attachment counts. The three existing VPS hosts are to receive PTY Home; the existing Mac Home and client identity remain valid. Existing certificates and FlowSplice transport compatibility must be retained. B, C and initial-v1 are unselected historical studies.

The first catalog is privately packaged with signed, independently authorized descriptors for each Home. Profiles have isolated installation state and credentials; the default profile retains the existing installation and secure-store account. Browsing and terminal tab switches do not disconnect other Homes. Background lifecycle remains the existing foreground-only policy.

PTY compatibility uses new explicitly requested list_details/new_named operations and a session_details reply. Existing Hello, List, New, Attached and Session messages stay unchanged. Display names and latest successful join times are stored with tmux using private user options; the internal UUID target remains unchanged. Names may repeat, are trimmed, and accept at most 64 Unicode scalars / 256 UTF-8 bytes without controls. No rename or deletion operation is added.

The owner approval supersedes the design-only state in earlier review files. Static boards remain visual references, not acceptance evidence. Implementation, tests, deployment and notarized packaging must be verified separately.

## Later authorization refinement

The owner subsequently requested readable device/host information plus PTY and explicit multiple authorization ranges. Independently authorized profiles describe the current tested implementation. The choice between one approval for several selected Home/service ranges and separate approvals remains pending; no Global scope is inferred. Native writer labels already use the system device name plus PTY. Issuer-side friendly credential names remain unfinished.
