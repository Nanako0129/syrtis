import AppKit
import SwiftUI

/// The dashboard shell on macOS 27+: a transparent borderless panel carrying
/// one `.regular` Liquid Glass surface, shown and dismissed by the menu bar's
/// expanded-interface session (`NSStatusItem.expandedInterfaceDelegate`).
///
/// NSPopover blurs the desktop with its own material before the content sees
/// it, so card glass inside a popover can only refract a flat tone. A
/// transparent panel lets the glass refract the real wallpaper; the macOS 27
/// session API supplies the menu-bar tracking (outside clicks on other menu
/// extras, keyboard navigation) that a hand-rolled panel used to fake with
/// `windowDidResignKey`, which closed the panel whenever a picker inside it
/// took key. See docs/knowledge/history/liquid-glass-experiments.md.
@MainActor
final class GlassPanelPresenter {
    private let panel = GlassPanel()
    private weak var anchor: NSStatusBarButton?
    private var cancelSession: (() -> Void)?
    /// The status item whose session is adopted. Sessions of different items
    /// can overlap while the menu bar hands over (begin of the new item and
    /// end of the old one arrive in either order), so an end is honoured only
    /// from the item that owns the current session.
    private weak var sessionOwner: AnyObject?
    private var eventMonitors: [Any] = []
    /// Bumped on every present so a close fade that finishes after a reopen
    /// doesn't order the reopened panel out.
    private var generation = 0

    /// Called once the panel is ordered out, so the owner can swap the live
    /// view for a placeholder and stop its `.task` loops.
    var onHidden: (() -> Void)?

    /// Shown from present until close starts: a panel still fading out reads
    /// as closed, so a click during the fade reopens it.
    var isShown: Bool { anchor != nil }

    init(contentViewController: NSViewController) {
        panel.contentViewController = contentViewController
        panel.contentView?.wantsLayer = true
        // The SwiftUI glass is rounded but the content view is not: its square
        // corners showed a dark rectangular rim and a square window shadow.
        // Clipping the layer leaves the corners truly empty, so the shadow,
        // computed from the window's alpha, follows the rounded shape.
        if let layer = panel.contentView?.layer {
            layer.cornerRadius = GlassPanelStyle.cornerRadius
            layer.cornerCurve = .continuous
            layer.masksToBounds = true
        }
        panel.onPerformClose = { [weak self] in self?.close() }
    }

    var hasSession: Bool { cancelSession != nil }

    /// Records the session the menu bar opened for `owner`, so `close()` can
    /// end it.
    func adopt(_ owner: AnyObject, cancelSession: @escaping () -> Void) {
        sessionOwner = owner
        self.cancelSession = cancelSession
    }

    /// Ends the menu bar session when there is one (its end callback then
    /// calls `sessionDidEnd`), otherwise hides directly.
    func close() {
        if let cancelSession { cancelSession() } else { hide() }
    }

    func sessionDidEnd(for owner: AnyObject) {
        guard owner === sessionOwner else { return }
        sessionOwner = nil
        cancelSession = nil
        hide()
    }

    /// Hides without ending a session — for handing the panel over to a
    /// session another item just began.
    func dismiss() {
        hide()
    }

    /// `layout` must already have the live content installed.
    func present(from button: NSStatusBarButton, height: CGFloat) {
        anchor = button
        layout(height: height, animate: false)
        generation += 1
        panel.alphaValue = 0
        panel.makeKeyAndOrderFront(nil)
        // Pay the first SwiftUI layout + draw (measured 65-125 ms) while the
        // panel is invisible, then animate on the next turn. Measured: more
        // main-thread work lands after that first draw (160-300 ms gaps), so
        // the animation runs in Core Animation, which the render server drives
        // — an NSWindow.animator fade lost every frame to that work.
        panel.contentView?.layoutSubtreeIfNeeded()
        panel.displayIfNeeded()
        let generation = generation
        DispatchQueue.main.async { [weak self] in
            guard let self, self.generation == generation, self.isShown else { return }
            self.animateIn()
        }
        installEventMonitors()
    }

    /// Top edge pinned just under the anchor button.
    func layout(height: CGFloat, animate: Bool) {
        guard let button = anchor, let window = button.window else { return }
        let anchorRect = window.convertToScreen(button.convert(button.bounds, to: nil))
        let frame = Self.frame(
            anchor: anchorRect, visible: window.screen?.visibleFrame,
            width: PopoverChrome.width, height: height)
        panel.setFrame(frame, display: true, animate: animate)
        panel.invalidateShadow()
    }

    /// Centered under the anchor, clamped inside the visible frame with a
    /// margin, top edge `GlassPanelStyle.menuBarGap` below the anchor.
    nonisolated static func frame(
        anchor: NSRect, visible: NSRect?, width: CGFloat, height: CGFloat
    ) -> NSRect {
        var x = anchor.midX - width / 2
        if let visible {
            let margin = GlassPanelStyle.screenMargin
            x = min(max(x, visible.minX + margin), visible.maxX - width - margin)
        }
        let top = anchor.minY - GlassPanelStyle.menuBarGap
        return NSRect(x: x, y: top - height, width: width, height: height)
    }

    func tearDown() {
        removeEventMonitors()
        panel.orderOut(nil)
        panel.contentViewController = nil
    }

    private func hide() {
        removeEventMonitors()
        anchor = nil
        let generation = generation
        let finish = { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.generation == generation else { return }
                self.panel.orderOut(nil)
                self.panel.contentView?.layer?.opacity = 1
                self.onHidden?()
            }
        }
        guard panel.isVisible, let layer = panel.contentView?.layer else {
            finish()
            return
        }
        CATransaction.begin()
        CATransaction.setCompletionBlock(finish)
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 1
        fade.toValue = 0
        fade.duration = GlassPanelStyle.closeDuration
        fade.timingFunction = CAMediaTimingFunction(name: .easeIn)
        layer.opacity = 0 // model value; the animation shows the fade
        layer.add(fade, forKey: Self.closeAnimationKey)
        CATransaction.commit()
    }

    private func animateIn() {
        guard let layer = panel.contentView?.layer else {
            panel.alphaValue = 1
            return
        }
        CATransaction.begin()
        CATransaction.setCompletionBlock { [weak self] in
            MainActor.assumeIsolated { self?.panel.invalidateShadow() }
        }
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 0
        fade.toValue = 1
        let slide = CABasicAnimation(keyPath: "transform.translation.y")
        slide.fromValue = GlassPanelStyle.openSlide
        slide.toValue = 0
        let group = CAAnimationGroup()
        group.animations = [fade, slide]
        group.duration = GlassPanelStyle.openDuration
        group.timingFunction = CAMediaTimingFunction(name: .easeOut)
        layer.removeAnimation(forKey: Self.closeAnimationKey)
        layer.opacity = 1 // a reopen mid-fade skipped the close's reset
        layer.add(group, forKey: "open")
        panel.alphaValue = 1
        CATransaction.commit()
    }

    private static let closeAnimationKey = "close"

    /// The session API leaves clicks in other windows and Esc to the app.
    /// Menu-bar clicks on other extras end the session by themselves.
    private func installEventMonitors() {
        guard eventMonitors.isEmpty else { return }
        if let global = NSEvent.addGlobalMonitorForEvents(
            matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown],
            handler: { [weak self] _ in MainActor.assumeIsolated { self?.close() } })
        {
            eventMonitors.append(global)
        }
        if let local = NSEvent.addLocalMonitorForEvents(matching: .keyDown, handler: { [weak self] event in
            guard event.keyCode == 53 else { return event } // Esc
            MainActor.assumeIsolated { self?.close() }
            return nil
        }) {
            eventMonitors.append(local)
        }
        // Clicks in this app's own ordinary windows (Settings, Sparkle's
        // update window) never reach the global monitor. Anything at or above
        // status-bar level is the menu bar's business, not an outside click.
        if let local = NSEvent.addLocalMonitorForEvents(
            matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown],
            handler: { [weak self] event in
                MainActor.assumeIsolated {
                    if let self, let window = event.window, window !== self.panel,
                       window.level.rawValue < NSWindow.Level.statusBar.rawValue
                    {
                        self.close()
                    }
                }
                return event
            })
        {
            eventMonitors.append(local)
        }
    }

    private func removeEventMonitors() {
        for monitor in eventMonitors { NSEvent.removeMonitor(monitor) }
        eventMonitors.removeAll()
    }
}

/// Visual parameters, each tuned by eye against the real screen. The round
/// notes record why a value is what it is so a later pass edits one line.
enum GlassPanelStyle {
    /// Panel corner, eyeballed against the macOS 27 Wi-Fi dropdown.
    static let cornerRadius: CGFloat = 16
    /// Gap between the menu bar and the panel's top edge.
    static let menuBarGap: CGFloat = 6
    /// Keeps the panel off the screen edge when the item sits near a corner.
    static let screenMargin: CGFloat = 8
    /// Open: fade plus a short drop from the menu bar, like system dropdowns.
    static let openDuration: TimeInterval = 0.18
    static let closeDuration: TimeInterval = 0.12
    static let openSlide: CGFloat = 8
    /// Light mode only: over light .regular glass, the shipping secondary and
    /// tertiary text read washed out and the white-lifted cards too bright.
    /// Dark mode keeps the shipping values. (A .regular tint of black 0.25 to
    /// darken the panel was tried and rejected — untinted reads better.)
    static let lightSecondaryText = Color.black.opacity(0.70)
    static let lightTertiaryText = Color.black.opacity(0.50)
    static let lightCardScrim = Color.black.opacity(0.04)
    /// Settings › Glass tint (#490). `.regular` glass follows the content behind
    /// it: in dark mode over a light window the panel turns light grey while the
    /// text keeps its dark-mode colours (the reporter's teal wordmark nearly
    /// vanished), and the reverse in light mode. The slider lays black (dark
    /// mode) or white (light mode) over the glass so the panel stays on its own
    /// mode's side. Default 0 is the shipping look: an always-on black 0.25 was
    /// tried earlier and rejected, so the user chooses.
    static let glassTintKey = "tokenbar.glass.tint"
    /// Full slider = 0.5 opacity: enough to hold the mode against any window
    /// behind it on the maintainer's check, while the glass still blurs through.
    static let maxGlassTint = 0.5
    /// Slider value (0...1, clamped) → tint opacity.
    static func glassTintOpacity(_ amount: Double) -> Double {
        min(max(amount.isFinite ? amount : 0, 0), 1) * maxGlassTint
    }
    /// Hover tooltips under the panel: .regular glass over a scrim that keeps
    /// the text legible. 0.20 read too see-through; 0.35 is the second round.
    /// The glass was .clear until #398: it barely blurs, so a tooltip over a
    /// list showed the rows beneath it sharp through the scrim. The maintainer
    /// compared both on the live panel and chose .regular. Both were seen to
    /// follow the system glass setting (NSGlassTintAmount, no public API).
    /// The popover separators (under the lens row, above the footer). The
    /// system `Divider` is a solid hairline that reads as a hard edge across
    /// the glass; a user found it abrupt ("很突兀"), and the maintainer's
    /// target is a quieter line. Light mode only: round 2 (0.08) was too faint
    /// to see, round 3 (0.14) sits between that and the system divider. Dark
    /// mode keeps the system divider, as v2.0.1 drew it, at the maintainer's
    /// call after comparing the rounds on the live panel.
    static let lightDivider = Color.black.opacity(0.14)
    static let tooltipScrimDark = Color.black.opacity(0.35)
    static let tooltipScrimLight = Color.white.opacity(0.35)
    /// Gray tooltip text over see-through glass washed out; lifting the text
    /// keeps the transparency, where a darker scrim would give it back.
    static let tooltipSecondaryDark = Color.white.opacity(0.80)
    static let tooltipTertiaryDark = Color.white.opacity(0.60)
    static let tooltipSecondaryLight = Color.black.opacity(0.75)
    static let tooltipTertiaryLight = Color.black.opacity(0.55)
    /// SegmentedPicker under the panel: capsule track plus a raised thumb in
    /// the macOS 26 segmented shape. Plain fills — glass nested inside the
    /// card's glass renders murky (see SegmentedPicker).
    static let segmentTrack: Double = 0.07
    static let segmentThumbDark = Color.white.opacity(0.18)
    static let segmentThumbLight = Color.white.opacity(0.90)
    static let segmentThumbShadow: Double = 0.15
    /// Tab-row selection thumb (client tabs, lens tabs) under the panel. A
    /// share of `.primary`, not `.quaternary`: over a bright wallpaper the glass
    /// turns its content dark while the app stays in dark mode, and
    /// `.quaternary` kept resolving for dark mode, so the thumb went dark gray
    /// behind black labels (seen 2026-09-26). `.primary` follows the label
    /// color the glass chose, so the thumb is a light lift under light
    /// content and a dark one under dark content. 0.14 is the first value the
    /// maintainer accepted on the live panel.
    static let tabThumbShare: Double = 0.14
    /// Selection thumb slide (segmented pickers, tab rows) and the crossfade
    /// or reflow of a card's content when its header toggle changes.
    static let selectionSlide: TimeInterval = 0.25
    static let contentSwitch: TimeInterval = 0.22
    /// The window chart's line stretching or shrinking into another window's.
    /// 0.4 and then 0.25 read too slow.
    static let spanMorph: TimeInterval = 0.2
    /// Canvas bar charts redraw whole and cannot tween old heights to new,
    /// so a metric switch regrows the bars from the baseline instead.
    static let chartRegrow: TimeInterval = 0.35
}

private struct InGlassPanelKey: EnvironmentKey {
    static let defaultValue = false
}

extension EnvironmentValues {
    /// True under the glass panel: the popover backdrop steps aside for the
    /// panel's own glass and cards take the light-mode scrim.
    var inGlassPanel: Bool {
        get { self[InGlassPanelKey.self] }
        set { self[InGlassPanelKey.self] = newValue }
    }
}

/// The panel's surface around PopoverView. The glass sits BEHIND the content
/// rather than wrapping it: content inside `.glassEffect` gets vibrancy,
/// which washed secondary text out in light mode.
struct GlassPanelSurface: ViewModifier {
    @Environment(\.colorScheme) private var colorScheme
    @AppStorage(GlassPanelStyle.glassTintKey) private var glassTint = 0.0

    func body(content: Content) -> some View {
        let shape = RoundedRectangle(cornerRadius: GlassPanelStyle.cornerRadius)
        if #available(macOS 26.0, *) {
            styledText(content.environment(\.inGlassPanel, true))
                .clipShape(shape)
                .background {
                    Rectangle().fill(.clear)
                        .glassEffect(
                            glass,
                            in: .rect(cornerRadius: GlassPanelStyle.cornerRadius))
                }
        } else {
            content
        }
    }

    @available(macOS 26.0, *)
    private var glass: Glass {
        let opacity = GlassPanelStyle.glassTintOpacity(glassTint)
        guard opacity > 0 else { return .regular }
        return .regular.tint((colorScheme == .dark ? Color.black : Color.white).opacity(opacity))
    }

    @ViewBuilder
    private func styledText(_ content: some View) -> some View {
        if colorScheme == .light {
            content.foregroundStyle(
                Color.primary, GlassPanelStyle.lightSecondaryText, GlassPanelStyle.lightTertiaryText)
        } else {
            content
        }
    }
}

/// Under the panel, animates on `value` changes. Keyed on the value rather
/// than `withAnimation` because most selections are @AppStorage, whose round
/// trip through UserDefaults drops the transaction — the thumb jumped instead
/// of sliding. Outside the panel the modifier is absent rather than
/// `.animation(nil)`: nil strips the transaction, which removed the popover's
/// own 0.16 s tab crossfade.
private struct PanelValueAnimation<Value: Equatable>: ViewModifier {
    let animation: Animation
    let value: Value
    @Environment(\.inGlassPanel) private var inGlassPanel

    func body(content: Content) -> some View {
        if inGlassPanel {
            content.animation(animation, value: value)
        } else {
            content
        }
    }
}

/// A tab row's selected background. Under the panel it is one thumb that
/// slides between tabs; outside it is the original per-tab fill, because a
/// matched geometry left in place would slide under the popover's own tab
/// animation too.
struct SelectionBackground<S: Shape>: ViewModifier {
    let isSelected: Bool
    let shape: S
    let namespace: Namespace.ID
    @Environment(\.inGlassPanel) private var inGlassPanel

    func body(content: Content) -> some View {
        if inGlassPanel {
            content.background {
                if isSelected {
                    shape.fill(Color.primary.opacity(GlassPanelStyle.tabThumbShare))
                        .matchedGeometryEffect(id: "thumb", in: namespace)
                }
            }
        } else {
            content.background(
                isSelected ? AnyShapeStyle(.quaternary) : AnyShapeStyle(.clear), in: shape)
        }
    }
}

extension View {
    /// Slides a `matchedGeometryEffect` selection thumb to the new value.
    func panelSelectionSlide<Value: Equatable>(_ value: Value) -> some View {
        modifier(PanelValueAnimation(
            animation: .snappy(duration: GlassPanelStyle.selectionSlide), value: value))
    }

    /// Animates a card's content when a header toggle changes `value`.
    func panelSwitchAnimation<Value: Equatable>(_ value: Value) -> some View {
        modifier(PanelValueAnimation(
            animation: .easeOut(duration: GlassPanelStyle.contentSwitch), value: value))
    }
}

/// Regrows a Canvas chart from its baseline when `value` changes, under the
/// panel only, by revealing it bottom-up through a mask. `keyframeAnimator`
/// owns the 0 → 1 run, so the reset and the growth cannot be coalesced into
/// one no-op update. The reset must be a MoveKeyframe: a zero-duration
/// LinearKeyframe interpolated to NaN and then -inf (measured per frame), which
/// blanked the chart for the whole run and, fed to a scaleEffect, aborted
/// AppKit on a non-finite NSView frame transform.
private struct PanelChartRegrow<Value: Equatable>: ViewModifier {
    let value: Value
    @Environment(\.inGlassPanel) private var inGlassPanel

    func body(content: Content) -> some View {
        if inGlassPanel {
            content.keyframeAnimator(initialValue: 1.0, trigger: value) { view, revealed in
                view.mask(alignment: .bottom) {
                    GeometryReader { proxy in
                        Rectangle()
                            .frame(height: proxy.size.height * revealed)
                            .frame(maxHeight: .infinity, alignment: .bottom)
                    }
                }
            } keyframes: { _ in
                MoveKeyframe(0.0)
                CubicKeyframe(1.0, duration: GlassPanelStyle.chartRegrow)
            }
        } else {
            content
        }
    }
}

/// Crossfades a Canvas whose colors or lines change with `value` (heatmaps,
/// the window line chart), under the panel only. A Canvas redraws whole, so
/// the implicit card animation has nothing to tween; swapping identity lets
/// the old drawing fade out under the new one.
private struct PanelCrossfade<Value: Hashable>: ViewModifier {
    let value: Value
    @Environment(\.inGlassPanel) private var inGlassPanel

    func body(content: Content) -> some View {
        if inGlassPanel {
            ZStack {
                content
                    .id(value)
                    .transition(.opacity)
            }
            .animation(.easeOut(duration: GlassPanelStyle.contentSwitch), value: value)
        } else {
            content
        }
    }
}

extension View {
    func panelCrossfade<Value: Hashable>(_ value: Value) -> some View {
        modifier(PanelCrossfade(value: value))
    }

    func panelChartRegrow<Value: Equatable>(_ value: Value) -> some View {
        modifier(PanelChartRegrow(value: value))
    }
}

/// The chart hover tooltips' chrome. Outside the panel it is the original
/// .regularMaterial with a quaternary border; under the panel it is glass,
/// whose own rim replaces the border.
private struct TooltipSurface: ViewModifier {
    @Environment(\.inGlassPanel) private var inGlassPanel
    @Environment(\.colorScheme) private var colorScheme

    func body(content: Content) -> some View {
        let shape = RoundedRectangle(cornerRadius: 8)
        if inGlassPanel, #available(macOS 26.0, *) {
            let dark = colorScheme == .dark
            content
                .foregroundStyle(
                    Color.primary,
                    dark ? GlassPanelStyle.tooltipSecondaryDark : GlassPanelStyle.tooltipSecondaryLight,
                    dark ? GlassPanelStyle.tooltipTertiaryDark : GlassPanelStyle.tooltipTertiaryLight)
                .background(
                    dark ? GlassPanelStyle.tooltipScrimDark
                                         : GlassPanelStyle.tooltipScrimLight,
                    in: shape)
                .glassEffect(.regular, in: .rect(cornerRadius: 8))
        } else {
            content
                .background(.regularMaterial, in: shape)
                .overlay(shape.strokeBorder(.quaternary))
        }
    }
}

extension View {
    func tooltipSurface() -> some View {
        modifier(TooltipSurface())
    }
}

/// Footer buttons take the glass button style under the panel; the popover
/// below macOS 27 keeps the bordered default.
struct PanelFooterButton: ViewModifier {
    @Environment(\.inGlassPanel) private var inGlassPanel

    func body(content: Content) -> some View {
        if inGlassPanel, #available(macOS 26.0, *) {
            content.buttonStyle(.glass)
        } else {
            content
        }
    }
}

final class GlassPanel: NSPanel {
    init() {
        super.init(
            contentRect: .zero,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered, defer: true)
        isOpaque = false
        backgroundColor = .clear
        hasShadow = true
        level = .popUpMenu
        hidesOnDeactivate = false
        isReleasedWhenClosed = false
        isMovable = false
        collectionBehavior = [.canJoinAllSpaces, .transient, .ignoresCycle]
    }

    override var canBecomeKey: Bool { true }

    /// PopoverView closes its window with performClose (the settings button,
    /// Esc), which a borderless panel ignores for lack of a close button.
    var onPerformClose: (() -> Void)?
    override func performClose(_ sender: Any?) { onPerformClose?() }
}

/// The popover's horizontal separators: a quieter hairline than the system
/// `Divider` under the glass panel in light mode (see
/// `GlassPanelStyle.lightDivider`); the system divider, unchanged, in dark mode
/// and in the NSPopover.
struct PanelDivider: View {
    @Environment(\.inGlassPanel) private var inGlassPanel
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        if inGlassPanel, colorScheme == .light {
            Rectangle()
                .fill(GlassPanelStyle.lightDivider)
                .frame(height: 1 / max(displayScale, 1))
        } else {
            Divider()
        }
    }
}
