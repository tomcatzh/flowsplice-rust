# Product and Platform Facts

## Existing FlowSplice behavior

- The Apple client is the iPhone/iPad counterpart of the completed Android Travel app and must use the same Rust Travel Core behavior.
- First-run enrollment collects Travel ID, Home ID, Relay address, private-key password, and confirmation. Device keys are generated locally; Home approval completes enrollment.
- Pending enrollment exposes a verification code and survives leaving/reopening the app.
- Once installed, the client starts/stops Travel, reports connection health, uptime, upload/download totals, active flows, and relay count.
- The Home service catalog supplies selectable homes, services, and protocols. A local mapping binds the selected service to `127.0.0.1:<port>`.
- Network-path changes must trigger reconnection. Durable installation and credential state must survive lock, suspension, relaunch, and process recreation.

## Apple platform facts

- Apple describes `NWPathMonitor` as an observer for monitoring and reacting to network changes: <https://developer.apple.com/documentation/Network/NWPathMonitor>
- UIKit can suspend or disconnect background scenes, so durable state and foreground reconciliation are required: <https://developer.apple.com/documentation/uikit/managing-your-app-s-life-cycle>
- Apple recommends foreground transitions as the point to reload resources and refresh network-backed data: <https://developer.apple.com/documentation/uikit/preparing-your-ui-to-run-in-the-foreground>
- On iPadOS, sidebars suit peer sections when space permits; compact iPhone layouts should use more space-efficient navigation: <https://developer.apple.com/design/human-interface-guidelines/sidebars>
- Live Activities appear on iPhone and iPad Lock Screens and must not perform their own network access: <https://developer.apple.com/documentation/activitykit/displaying-live-data-with-live-activities>

## Prototype constraints

- These HTML files compare information architecture and visual direction only. Production controls will be SwiftUI-native (`NavigationSplitView`, `List`, `Form`, sheets, menus, toolbars, alerts).
- The examples use real FlowSplice terminology and the existing Continuous S asset. No invented service or network capability becomes a product commitment merely because it appears as representative data.
