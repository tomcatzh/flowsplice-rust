# FlowSplice PTY identity

One new terminal ribbon mark is shared by the PTY clients on Android, iOS/iPadOS and macOS. The mint `>_` symbol combines a terminal prompt with two joined streams and follows the PTY interface's green palette.

## Masters and exports

- `pty-mark.png`: original generated transparent artwork, 1254 × 1254. Used for Android's foreground and the macOS icon.
- `pty-icon.png`: opaque background edit of the same mark, 1254 × 1254. Used for iOS/iPadOS.
- Run `python3 scripts/export-pty-icons.py` from the repository root with Pillow installed to reproduce platform PNGs and asset catalog metadata. The exporter performs only resizing, platform padding and encoding; it does not redraw the mark.
- Android supplies its own launcher mask over the deep forest background. The foreground has a 12 dp inset in a 108 dp canvas to preserve the emblem inside the adaptive icon safe region. See [Android's adaptive icon guidance](https://developer.android.com/develop/ui/compose/system/icon_design_adaptive).
- Apple icons use named app icon sets in the shared catalog: an opaque 1024 px iOS master and macOS 16–512 pt at 1x/2x. See [Apple's app icon asset catalog format](https://developer.apple.com/library/archive/documentation/Xcode/Reference/xcode_ref-Asset_Catalog_Format/AppIconType.html).

The existing general FlowSplice brand generator owns its original assets; PTY exports are maintained separately.

## Generation provenance

Created on 2026-09-07 with Codex's built-in image generation tool, followed by one background edit. No fallback CLI was used. The built-in interface did not expose a selectable model ID. Output dimensions are recorded above rather than inferred from the prompt.

Original generation prompt:

```text
Use case: logo-brand.
Asset type: final square application icon master for the FlowSplice PTY family, shared by Android, iOS, iPadOS and macOS.
Primary request: design one original, refined terminal app identity that combines a command prompt with the idea of two encrypted streams being spliced together.
Composition: a single bold centered emblem, inspired by the terminal symbols > and _, formed from two interlocking broad geometric ribbon strokes with precise soft corners. It should read immediately as a terminal at 32 pixels. One cohesive mark, not a terminal window illustration. Keep the complete important emblem inside the central 56% width and central 48% height so a circular launcher mask never cuts it off.
Color palette: deep forest green background matching a terminal UI around #101916; mint green and pale sea-glass green strokes, with one restrained darker emerald joining facet. Crisp high contrast, restrained and professional. Subtle dimensionality is acceptable only within the emblem; no dramatic glow, no heavy shadows, no glass sphere, no metallic sheen.
Canvas: 1024 by 1024 pixels, square, opaque, background extends to every edge. Absolutely no baked rounded app tile, no border, no outer margin on white, no surrounding device/mockup, no perspective.
No written app name, no letters, no watermark, no lock, no shield. Deliver the icon itself as the complete image, exactly one finished design.
```

Opaque background edit prompt (the original transparent image was the edit target):

```text
Use case: precise-object-edit.
Input image 1 is the edit target: the FlowSplice PTY mint terminal ribbon icon.
Change only the background: replace all transparent pixels with a solid deep forest green #101916 background, extending fully to all four square edges. The entire output must be opaque, including corners. No transparent pixels, no checkerboard, no rounded outer tile.
Preserve the existing terminal >_ ribbon mark exactly: same geometry, color, lighting, size, position, folds, spacing and proportions. Keep its subtle light aura. Do not redraw, enlarge or reposition the mark. Do not add text, borders, extra objects or a device mockup. Deliver one square finished application icon, 1024 by 1024.
```
