---
status: active
id: kb-history-liquid-glass
kind: canonical
scope: repository
read_when: considering a glass, popover, panel, or transparency redesign
last_verified: 2026-10-09
sources: ["sanitized local experiment source", "sanitized Liquid Glass project memory", "Sources/Syrtis/GlassBackground.swift", "Sources/Syrtis/Views/Cards.swift", "Sources/Syrtis/GlassPanelPresenter.swift", "Sources/Syrtis/StatusItemController.swift", "macOS 27.0 SDK AppKit headers (NSStatusItem.h, NSGlassEffectView.h)", "2026-09-24/25 live panel spike rounds"]
---

# Liquid Glass experiments

## 文件目的

這份文件記錄 Liquid Glass 調查的 durable technical evidence。2026-06/07 的第一輪調查在 macOS 26.5 SDK 上撞到兩道結構性牆而 parked；2026-09 在 macOS 27.0 SDK 上重啟，新的公開 API 拆掉了第一道牆，macOS 27 以上的出貨外殼因此改為透明 panel。

> **結論（2026-09）：** macOS 27 的 `NSStatusItem.expandedInterfaceDelegate` 讓透明 `NSPanel` 取得選單列原生的開關與追蹤行為，panel 裡單一 `.regular` 玻璃能折射真實桌布。第二道牆（玻璃跟隨 app 深淺色、沒有 theme-independent material）仍在，以淺色專用的文字階層色與卡片 scrim 補足可讀性。macOS 26 以下維持 NSPopover 與原卡片配方。

## 目錄

- [Goal](#goal)
- [Verified findings](#verified-findings)
- [Decisive evidence](#decisive-evidence)
- [Glass variants](#glass-variants)
- [Approaches tried](#approaches-tried)
- [Structural gaps](#structural-gaps)
- [Shipping recipe](#shipping-recipe)
- [macOS 27 glass panel](#macos-27-glass-panel)
- [Known limits](#known-limits)

---

## Goal

The 2026-06/07 investigation asked why TokenBar's dashboard cards looked flat or smoked instead of like macOS Control Center or the Wi-Fi menu-bar dropdown, and whether a third-party menu-bar app could match those system panels.

## Verified findings

| Question | Verified answer |
|---|---|
| Is the card glass real Liquid Glass? | **Yes.** `.glassEffect` genuinely refracts contrasty content behind it. |
| What does the refraction look like? | Blur, dimming, and a thin rim-lens at the rounded edges—not dramatic geometric warping. |
| Why do the shipping cards look flat? | They sit over a spatially uniform `NSVisualEffectView.Material.hudWindow` backdrop, so the glass has little contrast to refract. |
| Does swapping `NSVisualEffectView.Material` help? | **No.** Every tested behind-window material is a uniform blur layer, with no spatial variation for the lens effect. |
| Is bundling or the SDK the gate? | **No.** A signed app built against the macOS 26.5 SDK using real `NSGlassEffectView` reproduced the same behavior. |
| `NSGlassEffectView` (AppKit) versus SwiftUI `.glassEffect`? | **Approximately equivalent.** AppKit's edge is marginally crisper, but there is no qualitative difference. |

## Decisive evidence

> **Decisive half-color/half-flat evidence:** The same card recipe placed half over colorful content and half over a flat dark area refracted the colorful half while the dark half stayed flat. The glass works; it needs contrasty content behind it.

A hover-tooltip z-order bug independently exposed the same behavior: the Streaks card's glass refracted the tooltip content beneath it. The observed effect is therefore content-dependent refraction, not a painted rim or a packaging artifact.

## Glass variants

| Variant | Behavior in this context | Decision |
|---|---|---|
| `.clear` | Ultra-transparent. Over a transparent window with no backing material it renders to almost nothing, so the card vanishes. It is too thin and lets the background read sharply without frost. | Not suitable as the panel base. |
| `.regular` | Frosts and blurs the background in the system-panel direction. It is the right visible base, although dark mode renders a dark frost. | Shipping panel base on macOS 27+ (see [macOS 27 glass panel](#macos-27-glass-panel)). |

## Approaches tried

| # | Approach | Result and decision |
|---:|---|---|
| 1 | Hand-drawn white rim on `.clear` cards | Rejected. It creates fake glass without real refraction. |
| 2 | Synthetic aurora gradient behind the cards inside the window | Works technically because the glass refracts it, but it is painted content rather than the real background the product needs. Rejected. |
| 3 | Transparent `NSPanel` replacing `NSPopover`, with cards using `.glassEffect(.clear)` | Cards are invisible because `.clear` over a transparent window is nearly nothing. Rejected. |
| 4 | The same transparent panel, with cards using `.glassEffect(.regular)` | Cards become visible and refract the desktop, but the panel chrome—header, footer, and gaps—becomes transparent. Raw desktop bleeds through and the result is messy. Rejected. |
| 5 | One `.regular` glass surface filling the panel, with cards as plain fills | Closest to the Wi-Fi-dropdown model: a cohesive frosted panel, readable base, and edge refraction. The panel surface shipped on macOS 27+; the plain card fills did not — cards keep the `.clear` recipe (see [macOS 27 glass panel](#macos-27-glass-panel)). |
| 6 | Flat white overlay, tuned from `0.18` to `0.07` | A flat white veil makes the surface too white and foggy; it fogs rather than brightens. Rejected. |
| 7 | White `.plusLighter` glow layer, even at `0.05` | It still fogs. An overlay is the wrong tool for brightness. Rejected. |
| 8 | Force the light material with `.environment(\.colorScheme, .light)` | It becomes a light-theme white surface instead of the theme-independent look of system panels. Rejected. |

## Structural gaps

| Gap | Consequence |
|---|---|
| `NSPopover` blurs the desktop before the cards see it. | `NSPopover` always draws its own material between the content and the desktop, so cards refract a flat blurred tone rather than sharp wallpaper. Reaching the desktop requires a transparent `NSPanel` and a real re-architecture: hand-rolled transient dismissal, positioning, and the missing system arrow all have to be rebuilt. |
| System panels use a theme-independent material, while public third-party glass follows the app color scheme. | Control Center and the Wi-Fi dropdown keep consistent translucency in light and dark mode. `.glassEffect` and `NSGlassEffectView` render a dark frost in dark mode, and public APIs provide no knob for the system's theme-independent material. This private-material gap remains even after a panel rewrite. |

> Even a perfect transparent-panel version would still be blur, dimming, and a rim over whatever is behind the popover. It would be subtle over a typical dark or plain desktop and would remain tied to the app theme. The dramatic, consistent system-panel look is not reachable with public APIs.

## Shipping recipe

Below macOS 27 the shipping app keeps the popover and this card recipe in `Sources/Syrtis/Views/Cards.swift`:

```swift
// GlassCardBackground (Views/Cards.swift), macOS 26 branch
content
    .background(colorScheme == .dark ? Color.black.opacity(0.32)
                                     : Color.white.opacity(0.10),
                in: RoundedRectangle(cornerRadius: cornerRadius))
    .glassEffect(.clear, in: .rect(cornerRadius: cornerRadius))
```

The backdrop remains `PopoverBackdrop` backed by `NSVisualEffectView(.hudWindow, .behindWindow)`. Do not use a white or `.plusLighter` overlay to brighten the glass: brightness has to come from the material, and the material follows the app theme—the second structural gap above.

## macOS 27 glass panel

The 2026-09 resumption checked the macOS 27.0 SDK against both walls. `Glass` still offers only `.regular`, `.clear`, and `.identity` plus `tint` and `interactive`, and `NSGlassEffectView.Style` still has Regular and Clear (the only addition is `effectIsInteractive`), so the second wall stands. The first wall fell to a new API: `NSStatusItem.expandedInterfaceDelegate` and `NSStatusItemExpandedInterfaceSession` let a status item that shows its own window participate in menu-bar tracking. The earlier spike's unsolved edge — a picker inside the panel resigning key and closing it through `windowDidResignKey` — no longer exists, because the session, not key status, decides when the panel closes.

`GlassPanelPresenter` (`Sources/Syrtis/GlassPanelPresenter.swift`) owns the panel; `StatusItemController` sets itself as the delegate of the main item and every client item on macOS 27+ and routes each session to the presenter.

The API exists only in the macOS 27 SDK, so the app now builds with Xcode 27: `ci.yml` and `release.yml` run on GitHub's `xcode-27` image (a public preview as of 2026-09), which carries Xcode 27.0 build 27A266a. The previous `macos-26` image carries Xcode 26.6 and cannot compile the delegate. Compiling the panel out for an older SDK is not an option: it builds, runs, and ships a release without the panel, the #343 trap described in `GlassBackground.swift`.

| Concern | Shipping behavior | Evidence |
|---|---|---|
| Surface | Borderless, clear `NSPanel` at `.popUpMenu` level with the `.nonactivatingPanel` style. The presenter explicitly activates the app for native pinch delivery; the style alone does not describe app activation. One `.regular` glass sits **behind** PopoverView (`GlassPanelSurface`); `PopoverBackdrop` steps aside under `inGlassPanel`. Cards keep the shipping `.clear` recipe. | Plain card fills (approach 5) lost the card glass look; wrapping the content in `.glassEffect` applied vibrancy that washed out light-mode secondary text. |
| Open and close | The session callbacks show and hide the panel. The app still cancels the session itself on clicks in other apps' windows (global mouse monitor) and on Esc, as the header asks; clicks on other menu extras end the session by themselves. | Live log: sessions ended by another menu extra arrived before the app's own monitor fired. |
| Left and right clicks | Once the delegate is set, a left click on the main item sends no button action; the session opens the panel. A right click still sends the action and shows the quota menu, and the menu bar opens a session during that menu's tracking. | Measured on the main item. Client items keep a guard against a double open because their left-click action was not measured. |
| Quota menu versus session | A flag set around `showQuotaMenu()` keeps that session from showing the panel, and the session is cancelled only after the menu closes. `NSApp.currentEvent` inside the session callback is no discriminator: it read mouseExited or an undocumented type, not the click. | Cancelling during tracking dismissed the menu; showing the panel during tracking reopened it behind the menu. |
| Focus and close | `GlassPanel` routes Esc and `performClose` to the presenter. A normal close returns activation to the recorded external app only while Syrtis remains frontmost. Settings closes through an explicit window handoff; clicks on existing Settings or Sparkle windows also retain activation. Status-item handoffs retain the return target, and the presentation generation invalidates a queued restore after reopening. | Physical testing of #472 confirmed activation is required for pinch and identified the missing focus return. Focus restoration is independent of the fade; native session and focus behavior require desktop acceptance. |
| Animation | Fade plus an 8 pt drop, 0.18 s in and 0.12 s out, run as Core Animation on the content layer. The first SwiftUI layout and draw is paid while the panel is invisible before the animation starts. | Measured: first draw 65–125 ms, then 160–300 ms more main-thread work; `NSWindow.animator` lost every frame to it, while the render-server animation does not. |
| Light mode | Secondary text black 0.70, tertiary black 0.50, card scrim black 0.04 instead of the shipping white 0.10 lift. Dark mode keeps the shipping values. A `.regular.tint(black 0.25)` to darken the panel was rejected by eye. | Values live in `GlassPanelStyle` with the reason next to each. |
| Corners | The content view's layer is clipped to the glass corner radius (continuous curve). | Unclipped, its square corners showed a dark rectangular rim and a square window shadow around the rounded glass. |
| Controls | Footer buttons take `.buttonStyle(.glass)`. `SegmentedPicker` becomes a capsule track with a raised plain-fill thumb, and both tab rows get a thumb that slides with `matchedGeometryEffect`. Outside the panel every control keeps its original fill. | Glass inside the card's glass renders murky, which is why the segmented thumb is a plain fill. |
| Hover tooltips | The nine chart tooltips share `tooltipSurface()`: `.clear` glass over a 0.35 scrim, with secondary and tertiary text lifted (white 0.80/0.60 dark, black 0.75/0.55 light). Outside the panel it is the original `.regularMaterial` with a quaternary border. | A 0.20 scrim read too see-through; darkening the scrim further would have given back the transparency, so the text was lifted instead. |
| Selection and content animation | Selections animate with `.animation(_:value:)` keyed on the value, only under the panel. Canvas bar charts regrow bottom-up through a mask on a metric switch; heatmaps crossfade; the window chart's line mirrors point by point on remaining/used and resamples to stretch or shrink into another window's line. | Most selections are `@AppStorage`, whose UserDefaults round trip dropped a `withAnimation` transaction. Outside the panel the modifier is absent, not `.animation(nil)`, which strips the transaction and removed the popover's own 0.16 s tab crossfade. A zero-duration `LinearKeyframe` interpolated to NaN and then -inf (measured per frame): it blanked the chart and, fed to a `scaleEffect`, aborted AppKit on a non-finite NSView frame transform; the reset is a `MoveKeyframe`. A crossfade keyed inside the window chart's `GeometryReader` showed no fade; the cause was not isolated. |

## Known limits

| Limit | Status |
|---|---|
| Theme-dependent material | The second wall is unchanged: the glass follows the app's light or dark appearance and there is no public theme-independent material. |
| Small text over bright backgrounds | Over a near-white wallpaper the light glass is nearly white and the dashboard's 10 pt and smaller captions lose contrast, while system menus stay legible at 13 pt with even lighter secondary text. The gap is type size, not color; it predates the panel and is a separate product decision. |
| Open latency | The first SwiftUI draw of PopoverView (65–125 ms measured) happens before the panel appears, because the view is rebuilt on every open to restart its `.task` loops. |
| Window switch latency | Switching the window card's window waits 295–418 ms (measured) on the main thread in `DashboardModel.refreshWindowQuotaHalves()`, which rebuilds every window's curves, heatmaps and summaries although only the selection changed. It predates the panel and is left to a separate change. **Resolved by `perf/quota-window-switch`:** the time was curve reads, not the folds — each read parses the whole quota history store in the engine, and one pass read each window up to four times (14 reads, 134 ms of a 140 ms pass, release build, `--refresh-timing`). Curves are now read once per series per publication, and a scoped window's model filter asks once per model id; a switch measures 12–15 ms. |
| Popover path unobserved | The macOS 26-and-below popover path was checked by review only: a macOS 27 machine always takes the panel and there is no switch back. |
| Selftest scope | SelfTest covers the placement math and the close routing (`performClose`, session-first close, direct hide without a session); the menu-bar session needs a real status item and a click and is accepted live. |
