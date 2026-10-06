import AppKit
import SwiftUI
import TokenBarCore

/// Owns the NSStatusItem and its NSPopover (AppKit-hosted icon, SwiftUI
/// popover content — the codexbar pattern). On macOS 27+ the popover is
/// replaced by `GlassPanelPresenter`, driven by the menu bar's
/// expanded-interface session. Later phases talk to it through
/// `updateTitle(_:)` and `showPopover()`.
@MainActor
final class StatusItemController: NSObject {
    private let statusItem: NSStatusItem
    private let popover: NSPopover
    /// Source of truth for the popover's (user-adjustable) height.
    let chrome = PopoverChrome()
    private var defaultsObserver: NSObjectProtocol?
    private var closeObserver: NSObjectProtocol?
    private var host: NSHostingController<AnyView>?
    private var animationSurface: StatusItemAnimationSurface?
    private var hasPresentedIcon = false
    private var clientStatusItems: [String: NSStatusItem] = [:]
    private var lastClientPresentations: [String: ClientTray.Presentation] = [:]
    private var lastClientTextColors: [String: NSColor] = [:]
    private let routeMemory: StatusItemRouteMemory
    private var popoverAnchorIdentity: String?
    /// Non-nil on macOS 27+, where it replaces the popover.
    private var glassPanel: GlassPanelPresenter?
    /// True while the right-click quota menu tracks. The menu bar opens an
    /// expanded session during that tracking, which must not show the panel.
    /// (`NSApp.currentEvent` inside the session callback is no discriminator:
    /// it read mouseExited or an undocumented type there, not the click.)
    private var isShowingQuotaMenu = false

    private var isShown: Bool { glassPanel?.isShown ?? popover.isShown }

    override init() {
        let defaults = UserDefaults.standard
        routeMemory = StatusItemRouteMemory(
            mainClient: defaults.string(forKey: ClientTray.activeTabKey)
                ?? ClientTray.overviewTab,
            mainView: defaults.string(forKey: ClientTray.activeViewKey)
                ?? AppView.overview.rawValue)
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        popover = NSPopover()
        super.init()

        popover.behavior = .transient
        // Start on the same placeholder a close swaps in: presentPopover()
        // installs the live view on every open. A live PopoverView here kept
        // its .task polls and its view graph running, hidden, from launch until
        // the first close — measured at over half a core with the old
        // TimelineView LED in a demo run that never opened the popover.
        let host = NSHostingController(rootView: AnyView(
            Color.clear.frame(width: chrome.width, height: chrome.minHeight)))
        self.host = host
        // The SwiftUI root has a fixed frame; let the popover keep our size
        // instead of chasing intrinsic-size updates. The real size is set per
        // open in showPopover() against the status item's actual screen.
        host.sizingOptions = []
        popover.contentSize = NSSize(width: chrome.width, height: chrome.minHeight)
        if #available(macOS 27.0, *) {
            let presenter = GlassPanelPresenter(contentViewController: host)
            presenter.onHidden = { [weak self] in
                guard let self else { return }
                self.host?.rootView = AnyView(
                    Color.clear.frame(width: self.chrome.width, height: self.chrome.minHeight))
            }
            glassPanel = presenter
            statusItem.expandedInterfaceDelegate = self
        } else {
            popover.contentViewController = host
        }

        // Swap the live UI for a placeholder when the popover closes for any
        // reason (transient outside-click, Esc, programmatic close) — mirrors
        // the SettingsWindowController pattern. Without this, PopoverView's
        // .task loops run for the process lifetime because the hosting
        // controller persists across transient open/close cycles.
        closeObserver = NotificationCenter.default.addObserver(
            forName: NSPopover.didCloseNotification, object: popover, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                // didCloseNotification fires on a later runloop turn (after the
                // ~200ms close animation), by which point a fast close→reopen
                // may already have reinstalled the live view. Re-check on the
                // next turn and only blank if the popover is still closed, so a
                // reopen that landed meanwhile isn't swapped to the placeholder.
                DispatchQueue.main.async { [weak self] in
                    guard let self, !self.popover.isShown else { return }
                    self.popoverAnchorIdentity = nil
                    self.host?.rootView = AnyView(
                        Color.clear.frame(width: self.chrome.width, height: self.chrome.minHeight))
                }
            }
        }

        // The chrome model drives the popover window from three inputs: the
        // bottom drag handle, the settings slider, and the screen-size resolve.
        chrome.onResize = { [weak self, weak popover] height, live in
            if let glassPanel = self?.glassPanel {
                glassPanel.layout(height: height, animate: !live)
                return
            }
            guard let popover else { return }
            popover.animates = !live // 1:1 tracking mid-drag; animate otherwise
            popover.contentSize = NSSize(width: PopoverChrome.width, height: height)
        }
        // The settings window's slider writes the height default from another
        // window — mirror it onto a live popover.
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            // macOS can post this synchronously from registerDefaults() while
            // SwiftUI is laying out a newly opened popover. Resizing in that
            // AttributeGraph transaction aborts the process, so apply the
            // value-gated reload on the next main-queue turn.
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { self?.chrome.reloadFromDefaults() }
            }
        }

        if let button = statusItem.button {
            let placeholder = NSImage(size: .zero)
            placeholder.isTemplate = true
            placeholder.accessibilityDescription = "Syrtis"
            button.image = placeholder
            button.imagePosition = .imageLeft
            button.toolTip = "Syrtis"
            button.setAccessibilityLabel("Syrtis")
            button.target = self
            button.action = #selector(togglePopover(_:))
            // Right-click opens the quota-source menu (battery-icon pattern).
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
            animationSurface = StatusItemAnimationSurface(button: button)
        }
    }

    /// Supplies the latest quota payload for the right-click menu
    /// (AppDelegate wires this to the tray animator's cache).
    var quotaPayloadProvider: (() -> AgentUsagePayload?)?

    func setAnimatedFrames(_ frames: [NSImage], speed: Float) {
        guard !frames.isEmpty else {
            showFallbackIconIfNeeded()
            return
        }
        hasPresentedIcon = true
        animationSurface?.showAnimated(frames: frames, speed: speed)
    }

    func setAnimationSpeed(_ speed: Float) {
        animationSurface?.setSpeed(speed)
    }

    func setStaticIcon(_ image: NSImage, isTemplate: Bool) {
        hasPresentedIcon = true
        animationSurface?.showStatic(image, isTemplate: isTemplate)
    }

    func stopTrayAnimation() {
        animationSurface?.stopAnimation()
    }

    func setAppearanceChangeHandler(_ handler: (() -> Void)?) {
        animationSurface?.onAppearanceChange = handler
    }

    private func showFallbackIconIfNeeded() {
        guard !hasPresentedIcon,
              let image = NSImage(
                  systemSymbolName: "chart.bar.fill",
                  accessibilityDescription: "Syrtis")
        else { return }
        setStaticIcon(image, isTemplate: true)
    }

    /// Whether the menu bar around the item renders dark (picks the white
    /// frame set over the black one). Status-item buttons report *vibrant*
    /// appearances (NSAppearanceNameVibrantDark), which bestMatch against
    /// [.darkAqua, .aqua] misses — match on the name instead.
    var isDarkAppearance: Bool {
        let appearance = statusItem.button?.effectiveAppearance ?? NSApp.effectiveAppearance
        return appearance.name.rawValue.localizedCaseInsensitiveContains("dark")
    }

    private var lastTitleKey = ""

    /// Sets the text shown next to the menu-bar icon ("" = icon only). A
    /// color renders as an attributed title, with the user's text override
    /// taking precedence over the automatic quota color.
    func updateTitle(
        _ title: String, color: NSColor? = nil, quotaRemaining: Double? = nil, stale: Bool = false
    ) {
        guard let button = statusItem.button else { return }
        let color = MenuBarTextColor.applyingStale(
            MenuBarTextColor.resolve(automatic: color, quotaRemaining: quotaRemaining),
            stale: stale, quotaRemaining: quotaRemaining, dark: isDarkAppearance)
        // Leading space keeps a gap between the template icon and the text.
        let value = title.isEmpty ? "" : " \(title)"
        let key = "\(value)|\(color?.description ?? "")"
        if key != lastTitleKey {
            lastTitleKey = key
            Self.setTitle(value, color: color, on: button)
        }
        button.imagePosition = value.isEmpty ? .imageOnly : .imageLeft
        animationSurface?.scheduleLayout()
    }

    private static func setTitle(_ value: String, color: NSColor?, on button: NSStatusBarButton) {
        if let color, !value.isEmpty {
            button.attributedTitle = NSAttributedString(string: value, attributes: [
                .font: NSFont.menuBarFont(ofSize: 0),
                .foregroundColor: color,
            ])
        } else {
            // Explicitly discard a previous custom foreground before returning
            // the title to AppKit's appearance-aware native text rendering.
            button.attributedTitle = NSAttributedString(string: value)
            button.title = value
        }
    }

    func showPopover() {
        guard let button = statusItem.button else { return }
        guard !isShown else { return }
        let current = persistedRoute()
        applyRoute(routeMemory.activateMain(
            currentClient: current.clientId, currentView: current.view))
        presentPopover(from: button, identity: "main")
    }

    private func persistedRoute() -> StatusItemRouteMemory.Route {
        let defaults = UserDefaults.standard
        return StatusItemRouteMemory.Route(
            clientId: defaults.string(forKey: ClientTray.activeTabKey)
                ?? ClientTray.overviewTab,
            view: defaults.string(forKey: ClientTray.activeViewKey)
                ?? AppView.overview.rawValue)
    }

    private func applyRoute(_ route: StatusItemRouteMemory.Route) {
        UserDefaults.standard.set(route.clientId, forKey: ClientTray.activeTabKey)
        UserDefaults.standard.set(route.view, forKey: ClientTray.activeViewKey)
    }

    func closePopover() {
        popoverAnchorIdentity = nil
        if let glassPanel {
            glassPanel.close()
        } else {
            popover.performClose(nil)
        }
    }

    private func liveRoot() -> AnyView {
        let view = PopoverView(routeMemory: routeMemory).environmentObject(chrome)
        return glassPanel == nil ? AnyView(view) : AnyView(view.modifier(GlassPanelSurface()))
    }

    private func presentPopover(from button: NSStatusBarButton, identity: String) {
        guard !isShown else { return }
        // Reinstall the live PopoverView so its .task loops start fresh
        // (the previous close swapped it for a static placeholder).
        host?.rootView = liveRoot()
        // Size against the screen the status item actually lives on (reliable,
        // unlike NSScreen.main at launch with no key window) every time we open.
        let visible = (button.window?.screen ?? NSScreen.main)?.visibleFrame.height ?? 900
        chrome.resolve(visibleHeight: visible)
        if let glassPanel {
            glassPanel.present(from: button, height: chrome.height)
            popoverAnchorIdentity = identity
            return
        }
        // Accessory apps are never frontmost; activate so the transient
        // popover gets key status and closes on outside clicks.
        NSApp.activate(ignoringOtherApps: true)
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        popoverAnchorIdentity = identity
    }

    private func reanchorPopover(to button: NSStatusBarButton, identity: String) {
        guard popover.isShown else {
            presentPopover(from: button, identity: identity)
            return
        }
        // Re-clamp against the DESTINATION item's screen before moving: a height
        // the user chose on a taller display would otherwise stay taller than a
        // shorter display's visible frame and leave the popover clipped. Same
        // resolve the normal presentation path does.
        let visible = (button.window?.screen ?? NSScreen.main)?.visibleFrame.height ?? 900
        chrome.resolve(visibleHeight: visible)
        // NSPopover supports changing the relative anchor while shown. Keep the
        // live host and Dashboard tasks intact; only the native anchor moves.
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        popoverAnchorIdentity = identity
    }

    func reconcileClientItems(_ presentations: [ClientTray.Presentation]) {
        let ordered = presentations.sorted { $0.clientId < $1.clientId }
        let next = Dictionary(uniqueKeysWithValues: ordered.map { ($0.clientId, $0) })
        var textColors: [String: NSColor] = [:]
        for presentation in ordered {
            textColors[presentation.clientId] = MenuBarTextColor.resolve(
                automatic: nil, quotaRemaining: presentation.remainingPercent)
        }
        guard next != lastClientPresentations || textColors != lastClientTextColors else { return }

        for id in Array(clientStatusItems.keys) where next[id] == nil {
            removeClientItem(id)
        }
        // Only clients that actually own an item are recorded, so a client whose
        // icon failed to render stays absent and the next pass retries it.
        var reconciled: [String: ClientTray.Presentation] = [:]
        for presentation in ordered {
            let textColor = textColors[presentation.clientId]
            if let item = clientStatusItems[presentation.clientId] {
                if lastClientPresentations[presentation.clientId] != presentation
                    || lastClientTextColors[presentation.clientId] != textColor
                {
                    updateClientItem(item, presentation: presentation, textColor: textColor)
                }
            } else if !createClientItem(presentation, textColor: textColor) {
                continue
            }
            reconciled[presentation.clientId] = presentation
        }
        lastClientPresentations = reconciled
        lastClientTextColors = textColors
    }

    private func createClientItem(_ presentation: ClientTray.Presentation, textColor: NSColor?) -> Bool {
        guard let image = AgentIconView.statusItemImage(clientId: presentation.clientId) else {
            return false
        }
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.autosaveName = presentation.autosaveName
        if #available(macOS 27.0, *), glassPanel != nil { item.expandedInterfaceDelegate = self }
        clientStatusItems[presentation.clientId] = item
        updateClientItem(item, presentation: presentation, textColor: textColor, image: image)
        return true
    }

    private func updateClientItem(
        _ item: NSStatusItem,
        presentation: ClientTray.Presentation,
        textColor: NSColor?,
        image: NSImage? = nil
    ) {
        guard let button = item.button else { return }
        if let image { button.image = image }
        button.imagePosition = .imageLeft
        button.imageScaling = .scaleNone
        Self.setTitle(" \(presentation.valueText)", color: textColor, on: button)
        button.toolTip = presentation.toolTip
        button.setAccessibilityLabel(presentation.accessibilityLabel)
        button.setAccessibilityIdentifier(presentation.processIdentity)
        button.identifier = NSUserInterfaceItemIdentifier(presentation.processIdentity)
        button.target = self
        button.action = #selector(clientItemAction(_:))
        button.sendAction(on: [.leftMouseUp])
    }

    private func removeClientItem(_ clientId: String) {
        guard let item = clientStatusItems.removeValue(forKey: clientId) else { return }
        if popoverAnchorIdentity == ClientTray.processIdentity(for: clientId) {
            closePopover()
        }
        if let button = item.button {
            button.target = nil
            button.action = nil
            button.identifier = nil
            button.setAccessibilityIdentifier(nil)
            button.setAccessibilityLabel(nil)
            button.toolTip = nil
        }
        NSStatusBar.system.removeStatusItem(item)
    }

    @objc private func clientItemAction(_ sender: Any?) {
        guard let button = sender as? NSStatusBarButton,
              let clientId = clientStatusItems.first(where: { $0.value.button === button })?.key
        else { return }

        let current = persistedRoute()
        applyRoute(routeMemory.activateClient(
            clientId, currentClient: current.clientId, currentView: current.view))
        let identity = ClientTray.processIdentity(for: clientId)
        // The expanded-interface session opens the panel instead. Measured on
        // the main item: a left click sends no action once the delegate is
        // set. Not measured on client items, so guard against a double open.
        if glassPanel != nil { return }
        if popover.isShown {
            if popoverAnchorIdentity == identity {
                closePopover()
            } else {
                reanchorPopover(to: button, identity: identity)
            }
        } else {
            presentPopover(from: button, identity: identity)
        }
    }

    /// Clean teardown on app termination: drop the defaults observer, close
    /// the popover, and remove the status item so ControlCenter tears the
    /// menu-bar item down via a normal removal instead of an abrupt
    /// connection-invalidation — the latter left RunningBoard "waiting on
    /// exit context" for ~40s on quit (seen in the 2026-06-16 freeze logs).
    func tearDown() {
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        defaultsObserver = nil
        if let closeObserver { NotificationCenter.default.removeObserver(closeObserver) }
        closeObserver = nil
        if isShown { closePopover() }
        glassPanel?.tearDown()
        for clientId in Array(clientStatusItems.keys) {
            removeClientItem(clientId)
        }
        lastClientPresentations.removeAll()
        popover.contentViewController = nil
        animationSurface?.tearDown()
        animationSurface = nil
        NSStatusBar.system.removeStatusItem(statusItem)
    }

    @objc private func togglePopover(_ sender: Any?) {
        if NSApp.currentEvent?.type == .rightMouseUp {
            isShowingQuotaMenu = true
            showQuotaMenu()
            isShowingQuotaMenu = false
            // The session the menu bar opened during tracking is ended only
            // now: cancelling it while the menu tracked dismissed the menu.
            if let glassPanel, !glassPanel.isShown { glassPanel.close() }
            return
        }
        if glassPanel != nil { return } // left clicks arrive as sessions
        if popover.isShown {
            closePopover()
        } else {
            showPopover()
        }
    }

    // MARK: - Right-click quota-source menu

    /// "What does the icon track?" — Auto plus every known quota window with
    /// its live remaining percent, checkmark on the current pick. Writes the
    /// same defaults key the settings panel edits; the icon follows within a
    /// couple of seconds.
    private func showQuotaMenu() {
        let menu = NSMenu()
        let payload = quotaPayloadProvider?()
        let persistedSelection = UserDefaults.standard.string(forKey: TrayAnimator.quotaSourceKey)
            ?? QuotaResolver.auto
        let current = QuotaResolver.canonicalSelection(
            payload: payload, selection: persistedSelection)

        let header = NSMenuItem(
            title: "Menu bar tracks".localized, action: nil, keyEquivalent: "")
        header.isEnabled = false
        menu.addItem(header)

        func add(_ title: String, selection: String) {
            let item = NSMenuItem(
                title: title, action: #selector(pickQuotaSource(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = selection
            item.state = selection == current ? .on : .off
            menu.addItem(item)
        }
        add("Auto (tightest window)".localized, selection: QuotaResolver.auto)

        if let payload {
            // Primary account only: the persisted selection is one global
            // string with no account component (`QuotaResolver.selection`
            // encodes `clientId|cardId`, never an account), and two accounts
            // of the same client can offer identically-carded windows (both a
            // "session.v1"), so listing an extra account's window here would
            // draw a second, indistinguishable "Claude Code" section whose
            // checkmark and pick could not be told apart from the primary's.
            // An extra account's windows remain visible and selectable in the
            // Agent-limits overview; this menu targets the primary only.
            for agent in payload.agents where agent.error == nil && agent.accountKey == nil {
                let windows = agent.uniqueCardWindows
                guard !windows.isEmpty else { continue }
                menu.addItem(.separator())
                let name = NSMenuItem(
                    title: ClientRegistry.style(agent.clientId).displayName,
                    action: nil, keyEquivalent: "")
                name.isEnabled = false
                menu.addItem(name)
                for window in windows {
                    let left = "%lld%% left".localized(
                        Int(min(100, max(0, window.remainingPercent)).rounded()))
                    // `label` is display-only here; the persisted selection
                    // below is keyed by `cardId`.
                    add(
                        "\(window.label.localized) — \(left)",
                        selection: QuotaResolver.selection(
                            clientId: agent.clientId, cardId: window.cardId))
                }
            }
        } else {
            let loading = NSMenuItem(
                title: "Loading quotas…".localized, action: nil, keyEquivalent: "")
            loading.isEnabled = false
            menu.addItem(loading)
        }

        // Pop up via a transient menu assignment so the next left-click still
        // toggles the popover instead of re-opening the menu.
        statusItem.menu = menu
        statusItem.button?.performClick(nil)
        statusItem.menu = nil
    }

    @objc private func pickQuotaSource(_ sender: NSMenuItem) {
        guard let selection = sender.representedObject as? String else { return }
        UserDefaults.standard.set(selection, forKey: TrayAnimator.quotaSourceKey)
    }
}

// Needs the macOS 27 SDK (Xcode 27) to compile, and CI and release build with
// it. Do not wrap this in a compile-time check to build on an older SDK: that
// builds and runs, and ships an app without the glass panel — the #343 trap
// described in GlassBackground.swift.
@available(macOS 27.0, *)
extension StatusItemController: @preconcurrency NSStatusItemExpandedInterfaceDelegate {
    func statusItem(_ item: NSStatusItem, didBegin session: NSStatusItemExpandedInterfaceSession) {
        guard let glassPanel else { return session.cancel() }
        // Opened without a session (`--open-popover`): a click closes it.
        if glassPanel.isShown, !glassPanel.hasSession {
            session.cancel()
            closePopover()
            return
        }
        // Still up for another item whose end has not arrived yet: hand over.
        // That item's late end is ignored by the presenter's owner check.
        if glassPanel.isShown { glassPanel.dismiss() }
        glassPanel.adopt(item, cancelSession: { session.cancel() })
        // Ended by togglePopover once the quota menu closes.
        if isShowingQuotaMenu { return }
        guard let button = item.button else { return session.cancel() }
        let current = persistedRoute()
        if item === statusItem {
            applyRoute(routeMemory.activateMain(
                currentClient: current.clientId, currentView: current.view))
            presentPopover(from: button, identity: "main")
        } else if let clientId = clientStatusItems.first(where: { $0.value === item })?.key {
            applyRoute(routeMemory.activateClient(
                clientId, currentClient: current.clientId, currentView: current.view))
            presentPopover(from: button, identity: ClientTray.processIdentity(for: clientId))
        } else {
            session.cancel()
        }
    }

    func statusItemDidEndExpandedInterfaceSession(_ item: NSStatusItem, animated: Bool) {
        guard let glassPanel else { return }
        let wasShown = glassPanel.isShown
        glassPanel.sessionDidEnd(for: item)
        if wasShown, !glassPanel.isShown { popoverAnchorIdentity = nil }
    }
}
