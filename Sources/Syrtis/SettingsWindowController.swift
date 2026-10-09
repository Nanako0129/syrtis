import AppKit
import TokenBarCore
import SwiftUI

/// Owns the standalone settings window (gear button, Cmd-comma, `--settings`).
/// One window per app, kept alive across closes so its position persists;
/// `show()` re-fronts it. The popover stays `.transient` and uninvolved —
/// the window carries its own live preview instead of pinning the popover.
@MainActor
final class SettingsWindowController {
    static let shared = SettingsWindowController()

    private var window: NSWindow?
    // AnyView so the live UI can be swapped for a static placeholder on close.
    private var host: NSHostingController<AnyView>?
    private var closeObserver: NSObjectProtocol?

    /// A place in Settings a caller wants brought into view. The intro card
    /// uses it so "Open Settings" lands on the section it just described
    /// instead of the top of the first page.
    enum Destination {
        case discord
        case usageAttribution
        case dashboard

        var page: SettingsPanel.Page {
            switch self {
            case .discord: return .general
            case .usageAttribution: return .usageAttribution
            case .dashboard: return .dashboard
            }
        }

        /// Matched by a `.id(...)` on the section itself, or nil when
        /// selecting the page already puts the destination at the top —
        /// the usage-attribution page has exactly one section, so there is
        /// nothing further to scroll to.
        var anchor: String? {
            switch self {
            case .discord: return Self.discordAnchor
            case .usageAttribution, .dashboard: return nil
            }
        }

        /// The Discord section's `.id`, non-optional on purpose: tagging the
        /// section with `anchor` itself would give it an `Optional<String>`
        /// id, which never equals the `String` handed to `scrollTo`, and the
        /// intro card's "Open Settings" would stop scrolling to it.
        static let discordAnchor = "settings.section.discord"
    }

    /// For a control inside the popover: close the popover first, then show
    /// on the next runloop turn. Showing in the same turn as the
    /// popover's animated close puts both vibrant windows in one CoreAnimation
    /// transaction and the native switch thumbs lose their first frame; and
    /// leaving the popover open keeps its polling running behind Settings.
    func showFromPopover(
        scrollingTo destination: Destination? = nil, from popoverWindow: NSWindow? = nil
    ) {
        if let popover = popoverWindow ?? NSApp.keyWindow, popover !== window {
            if let panel = popover as? GlassPanel {
                panel.closeForWindowHandoff()
            } else {
                popover.performClose(nil)
            }
        }
        DispatchQueue.main.async { self.show(scrollingTo: destination) }
    }

    func show(scrollingTo destination: Destination? = nil) {
        let existing = self.window
        let window = existing ?? makeWindow(destination: destination)
        self.window = window
        // Reopening a kept-alive window: reinstall the live settings UI that
        // the previous close swapped out for a static placeholder. (Closing
        // only orders the window out; leaving the live content mounted let its
        // preview TimelineView(.periodic) keep re-rendering off-screen at up
        // to 40fps and pin a core in the background — the chronic CPU spin.)
        if existing != nil {
            // `.id` only when a destination is requested: a fresh identity
            // re-runs the view's `@State`, which is what selects the page and
            // fires the scroll — and is exactly what an ordinary reopen must
            // NOT do, since it would discard the page the user was last on.
            host?.rootView = destination.map {
                AnyView(
                    SettingsWindowView(destination: $0)
                        .id(($0.anchor ?? $0.page.rawValue) + UUID().uuidString))
            } ?? AnyView(SettingsWindowView())
        }
        let firstShow = !window.isVisible
        // Accessory apps are never frontmost; activate or the window opens
        // behind whatever app currently has focus.
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        // Dead-center on open (but never yank an already-open window).
        // NSWindow.center() sits noticeably above center, so place by hand —
        // and only after ordering front: the hosting view inflates the frame
        // by the title-bar safe area (580 -> 612) on its first layout, so
        // centering the pre-show frame sat ~16pt low. The move lands in the
        // same runloop turn, before the window is on screen.
        if firstShow {
            center(window)
        }
    }

    private func center(_ window: NSWindow) {
        guard let screen = window.screen ?? NSScreen.main ?? NSScreen.screens.first
        else { return }
        let visible = screen.visibleFrame
        window.setFrameOrigin(NSPoint(
            x: visible.midX - window.frame.width / 2,
            y: visible.midY - window.frame.height / 2))
    }

    private func makeWindow(destination: Destination? = nil) -> NSWindow {
        let host = NSHostingController(rootView: AnyView(SettingsWindowView(destination: destination)))
        self.host = host
        let window = NSWindow(contentViewController: host)
        // NSWindow(contentViewController:) sizes lazily (the frame is still
        // 1x0 at show time, which broke the centering math) — force the
        // SwiftUI fitting size up front.
        window.setContentSize(host.view.fittingSize)
        window.title = "Syrtis Settings".localized
        window.styleMask = [.titled, .closable, .miniaturizable, .fullSizeContentView]
        // The glass backdrop runs under the title bar (the popover look);
        // scroll views inset their content via the safe area.
        window.titlebarAppearsTransparent = true
        // macOS 26 draws the native title flush left next to the traffic lights
        // and an opaque title bar only trades the seamless glass for a solid
        // band without centering it, so show no title at all — the sidebar and
        // the window's own content already say what this is. `window.title`
        // stays set for Mission Control and the Window menu.
        window.titleVisibility = .hidden
        window.isReleasedWhenClosed = false
        // Swap the live UI for a static, same-size placeholder when the window
        // closes so its preview timelines + polling .tasks are torn down (a
        // kept-alive closed window otherwise keeps rendering in the
        // background); show() reinstalls the live UI on the next open.
        closeObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.host?.rootView = AnyView(
                    Color.clear.frame(
                        width: SettingsWindowView.contentSize.width,
                        height: SettingsWindowView.contentSize.height))
            }
        }
        // The hosting view inflates the frame by the title-bar safe area
        // (580 -> 612) in a layout pass after the first order-front that no
        // amount of layoutIfNeeded forces early — re-center once when it
        // lands so the first open sits dead-center (one-shot; later opens
        // start from the final size and never resize again).
        var token: NSObjectProtocol?
        token = NotificationCenter.default.addObserver(
            forName: NSWindow.didResizeNotification, object: window, queue: .main
        ) { [weak self] notification in
            if let token { NotificationCenter.default.removeObserver(token) }
            guard let window = notification.object as? NSWindow else { return }
            MainActor.assumeIsolated { self?.center(window) }
        }
        return window
    }
}
