# FlowSplice Icon Direction Spec

## Product Understanding

FlowSplice is not merely an Android utility and the symbol cannot be tailored only to the current Travel screen. It is the common identity for a Rust system that links a user's Travel device to explicitly chosen Home services through a set of Relay paths. The user experiences this as a simple local port that stays available while the system handles discovery, selection, reconnection, counters, and background continuity. The identity therefore needs to feel like dependable infrastructure without looking institutional, defensive, or enterprise-generic. “Flow” should be understood as movement and continuity; “Splice” should be understood as a deliberate join between paths. The mark should make those two ideas visible in one compact gesture.

The immediate application surfaces are unusually demanding. On Android the icon appears as a large launcher tile, a masked adaptive icon in several vendor shapes, a themed monochrome icon, a splash-screen symbol, and a much smaller system mark. On the Web it must remain identifiable as a 16-pixel browser tab favicon on desktop, as a saved-site icon on iPad and Android, and as a one-color Safari pinned-tab silhouette. A design that works only as a 512-pixel illustration fails this project. The master geometry must be vector-native, have a strong outer silhouette, avoid fragile gaps, and retain its meaning with color removed.

## Audience and Context

The primary audience is the owner/operator using FlowSplice across personal computers, a foldable Android phone, tablets, routers, and browser-based local control panels. The product is technical, but the mark should not be a developer-tool joke or a diagram that requires explanation. It should be calm enough to sit in a notification drawer and browser tab for hours, yet distinctive enough that the user can find it instantly among other system tools. Secondary audiences are future users installing the APK or opening the Home and Travel control pages.

## Emotional Tone

- Precise, quiet, resilient, and direct.
- Technical without cold corporate symbolism.
- Modern without generic glowing gradients.
- Friendly enough for a personal device, serious enough for infrastructure.
- More “well-made instrument” than “security product.”

## Required Content in Each Direction Board

Each direction must show the same functional evidence so the comparison is meaningful:

1. A large canonical mark.
2. Android-style previews under a circle and a rounded-square mask.
3. A one-color themed version.
4. A browser-tab example at approximately 16 pixels.
5. A compact size ladder demonstrating 64, 32, and 16 pixels.
6. A short statement of what the form means.
7. The exact palette used by that direction.

## Output Format and Size

- Three independent, self-contained HTML direction boards.
- Fixed review viewport: 1440 × 1000 pixels.
- One PNG screenshot per direction at the same viewport.
- Draft files live under `design-demos/icon-directions/`.
- These are decision artifacts only. Production Android and Web assets are generated after user approval.

## Constraints

- Use only colors already grounded in the repository palette.
- The critical mark geometry must fit inside the Android safe zone.
- Avoid literal infrastructure objects and conventional defensive imagery.
- Avoid letters as the entire idea; any resemblance to “F” or “S” must arise from flow geometry rather than typography.
- The mark must work as a one-color filled or stroked silhouette.
- No user data, deployment values, service names, or operational addresses appear in the boards.

## Visual Motif Hypothesis

The unique motif is the join itself. FlowSplice differs from a generic connection product because it exposes a chosen remote service as a stable local endpoint while the underlying path can change. The icon can therefore show either a continuous route that bends through a splice, two routes exchanging position at one controlled crossing, or several lanes cut and rejoined along a deliberate seam. Those three interpretations form the direction set.

## Three Direction Logic

- **A · Continuous S:** one route travels from one edge to another through a calm S-shaped bend. It emphasizes continuity and simplicity.
- **B · Exchange:** two independent paths cross and exchange endpoints. It emphasizes selection, redundancy, and an intentional splice.
- **C · Cutline:** three modular lanes are separated and rejoined by a diagonal seam. It emphasizes engineering, modularity, and a family-wide system.

## Assumptions

- There is no hidden legacy FlowSplice logo or external brand guide beyond the repository evidence.
- The current mint/dark palette has enough user recognition to remain the primary family palette.
- A wordmark is not part of this first decision; the icon must succeed alone.
- The user prefers a coherent project-wide identity over separate role-specific icons.
- After approval, role variants may be derived by small color or badge changes, but the base symbol remains identical.
