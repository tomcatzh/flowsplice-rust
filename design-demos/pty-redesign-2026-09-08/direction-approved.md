# Approved implementation direction — 2026-09-08

The owner selected **A: horizontal desktop tabs** and approved the revised phone workflow. The terminal uses the largest available area. macOS has no clickable virtual keys. Default connection reuses the existing securely stored private-key password and does not ask for a startup/login password.

Implementation is authorized for simultaneous multi-Home clients, the selected multi-page interface, naming before session creation, creation/latest-join timestamps and active attachment counts. The three existing VPS hosts are to receive PTY Home; the existing Mac Home and client identity remain valid. Existing certificates and FlowSplice transport compatibility must be retained. B, C and initial-v1 are unselected historical studies.

The current approved category flow uses one privately packaged service-class descriptor, one explicit approval, and a shared client identity. Matching current/future Business Homes are discovered through their signed grants. The earlier independently authorized profile format remains compatible and retains its existing installation and secure-store accounts. Browsing and terminal tab switches do not disconnect other Homes. Background lifecycle remains the existing foreground-only policy.

PTY compatibility uses new explicitly requested list_details/new_named operations and a session_details reply. Existing Hello, List, New, Attached and Session messages stay unchanged. Display names and latest successful join times are stored with tmux using private user options; the internal UUID target remains unchanged. Names may repeat, are trimmed, and accept at most 64 Unicode scalars / 256 UTF-8 bytes without controls. No rename or deletion operation is added.

The owner approval supersedes the design-only state in earlier review files. Static boards remain visual references, not acceptance evidence. Implementation, tests, deployment and notarized packaging must be verified separately.

## Later authorization refinement

The owner subsequently requested one application request covering every Home of the same service category, and explicitly authorized frontend/backend changes. PTY uses the exact `flowsplice.pty.v1` / TCP category. A Global issuer approves this ServiceClass credential; it is not a Global Travel grant. Current/future properly authorized matching Homes become available automatically. Device names plus PTY are bound to the full signed request and shown in approval/history. Old certificates are preserved without automatic permission widening. Validation and production rollout remain separate checkpoints.
