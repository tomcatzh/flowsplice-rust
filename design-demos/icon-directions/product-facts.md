# FlowSplice Icon Product Facts

Observed: 2026-08-24

## Product

- FlowSplice is the project and product name used throughout the repository.
- Its durable product metaphor is explicit service flow: a Travel client turns a Home service into a local port, with one or more Relays carrying the selected business flow.
- The product has four named roles: Server, Relay, Home, and Travel. The icon must represent the family, not only the Android Travel shell.
- The existing Home and Travel web interfaces use near-black green backgrounds, mint accents, pale text, thin borders, and compact operational typography.
- Existing workflow diagrams distinguish Travel with blue, Relay with violet, and Home with teal, while the business path transitions from blue to teal.
- The current Android launcher artwork is the Android template robot over a green construction grid. It is not a FlowSplice brand asset and should be replaced.
- Neither embedded web interface currently declares favicon assets.

## Current Platform Requirements

- Android adaptive icons use separate foreground and background layers, with an optional monochrome layer for themed launchers. All layers use a 108 × 108 dp canvas, while the critical mark remains inside the centered 66 × 66 dp safe zone. Source: [Android adaptive icons](https://developer.android.com/develop/ui/compose/system/icon_design_adaptive).
- Apple web clips can use PNG touch icons, including 152 × 152 for iPad and 180 × 180 for high-density iPhone screens. Source: [Safari Web Content Guide](https://developer.apple.com/library/archive/documentation/AppleApplications/Reference/SafariWebContent/ConfiguringWebApplications/ConfiguringWebApplications.html).
- Safari pinned tabs use a single-layer black SVG with a `0 0 16 16` viewBox and apply color from the HTML link element. Source: [Safari pinned tab icons](https://developer.apple.com/library/archive/documentation/AppleApplications/Reference/SafariWebContent/pinnedTabs/pinnedTabs.html).
- Web manifests can provide exact-size raster icons and scalable SVG icons, letting browsers select the most suitable asset. Source: [MDN manifest icons](https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Manifest/Reference/icons).

## Delivery Target After Direction Approval

- One canonical vector master.
- Android adaptive foreground, background, and monochrome resources plus legacy density exports.
- Web SVG favicon, multi-resolution ICO, 16/32/48 PNGs, 180 touch icon, 192/512 manifest icons, and a maskable variant.
- One monochrome pinned-tab asset.
- Shared HTML metadata and manifests for every embedded web interface.
