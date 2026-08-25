# Approved Direction

## Directions shown

- Direction A — Native Utility
  - Prototype: `direction-a-native-utility.html`
  - Review screenshot: `/tmp/flowsplice-direction-a.png`
- Direction B — Signal Console
  - Prototype: `direction-b-signal-console.html`
  - Review screenshot: `/tmp/flowsplice-direction-b.png`
- Direction C — Service Library
  - Prototype: `direction-c-service-library.html`
  - Review screenshot: `/tmp/flowsplice-direction-c.png`

## User selection

Exact user wording on 2026-08-25:

> A + B，做跟随系统日夜？

## Approved interpretation

- Use Direction A for information architecture: native Apple forms, compact iPhone navigation, and an iPad split view/sidebar.
- Use Direction B as the dark-appearance expression: Deep Ink surfaces, Flow Mint connection signals, and stronger visibility for reconnect/recovery events.
- Follow the system Light/Dark appearance automatically. Do not add an app-specific appearance override.
- Prefer SwiftUI semantic colors, materials, text styles, and SF Symbols. Keep hard-coded brand colors limited to FlowSplice identity/accent roles and provide adaptive contrast.
- Preserve the user-selected hybrid consistently across enrollment, overview, catalog/mapping, diagnostics, and recovery states.

## Form rationale

The form comes from the product’s two dominant jobs: configure a trusted device with low ambiguity, then understand connection/recovery state at a glance. Direction A carries the configuration and navigation grammar; Direction B carries the live operational state without turning the app into a web dashboard.
