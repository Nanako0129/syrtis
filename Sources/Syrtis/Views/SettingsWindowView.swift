import AppKit
import SwiftUI
import TokenBarCore

/// Standalone settings window: the settings form on the left, a live preview
/// column on the right. Every control writes UserDefaults and every preview
/// piece reads the same keys (plus the real menu bar reacts anyway), so
/// changes reflect instantly without touching the popover's transient
/// behavior.
/// Sidebar footer link: plain (not the loud system link blue), a pointing-hand
/// cursor AppKit does not give `Link` on its own, and a hover chip so two
/// adjacent links read as two separate targets.
private struct FooterLink: View {
    let title: String
    let systemImage: String
    let url: String

    @State private var hovering = false

    var body: some View {
        Link(destination: URL(string: url)!) {
            Label(title.localized, systemImage: systemImage)
                .font(.caption2)
                .foregroundStyle(hovering ? Color.primary : Color.secondary)
                .padding(.horizontal, 6)
                .padding(.vertical, 3)
                .background(
                    RoundedRectangle(cornerRadius: 5)
                        .fill(Color.primary.opacity(hovering ? 0.09 : 0)))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .hoverCursor(.pointingHand) { hovering = $0 }
    }
}

struct SettingsWindowView: View {
    static let contentSize = CGSize(width: 856, height: 580)

    // Settings manages process-wide tray preferences, so its client universe
    // stays all-time even when the dashboard is scoped to one saved year. The
    // model still defaults cachesSnapshot to false: it must never overwrite the
    // popover's restore snapshot with its independently polled data.
    @State private var model = DashboardModel(initialYear: nil)
    @State private var tokensPerMin: Double?
    /// Set only when a caller asked for a specific place; the intro card is
    /// the one caller today. Nil means an ordinary open, which must land
    /// wherever the user last was.
    var destination: SettingsWindowController.Destination?
    @State private var selectedPage = SettingsPanel.Page.menuBar
    /// Master switch: off hides the preview's Agent-limits card too.
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    /// Observed so the preview (tab list, limits card, trace card) re-derives
    /// the instant the user toggles visibility or reorders in the left panel,
    /// instead of lagging a poller tick behind.
    @AppStorage(ClientRegistry.tabHiddenKey) private var tabsHiddenRaw = ""
    @AppStorage(ClientRegistry.tabOrderKey) private var tabsOrderRaw = ""
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSourceRaw = QuotaResolver.auto
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""
    @AppStorage(ClaudeExtraRoots.generationKey) private var extraRootsGeneration = 0

    /// The user's hidden client set, parsed from the observed raw string.
    private var hiddenClients: Set<String> {
        ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(tabsHiddenRaw))
    }

    /// Reconcile again for every generated payload, selection, or exclusion
    /// change. Legacy/demo payloads have no publication generation, so their
    /// resolved scalar is the content fingerprint that prevents a timestamp
    /// collision from preserving stale defaults.
    private var quotaReconciliationID: String? {
        let excluded = ClientRegistry.quotaExcludedClients()
        let exclusionSignature = [
            excluded.sorted().joined(separator: ","),
            tabsHiddenRaw,
            limitsHiddenRaw,
        ].joined(separator: "|")
        return Self.quotaReconciliationID(
            payload: model.agentUsage,
            persistedSelection: quotaSourceRaw,
            excluding: excluded,
            exclusionSignature: exclusionSignature)
    }

    nonisolated static func quotaReconciliationID(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        exclusionSignature: String
    ) -> String? {
        guard let payload else { return nil }
        let payloadIdentity: String
        if let generation = payload.publicationGeneration {
            payloadIdentity = "generation:\(generation)"
        } else {
            let remaining = QuotaSelectionPolicy.resolveRemainingPercent(
                payload: payload,
                persistedSelection: persistedSelection,
                excluding: excluding,
                cachedRemaining: nil)
            let fingerprint = remaining.map { String($0.bitPattern) } ?? "nil"
            payloadIdentity = "legacy:\(payload.generatedAt):\(fingerprint)"
        }
        return [payloadIdentity, persistedSelection, exclusionSignature]
            .joined(separator: "|")
    }

    private static let pageTop = "settings.page.top"

    var body: some View {
        HStack(spacing: 0) {
            // The footer rides in the List's own safe area, not a sibling VStack:
            // .sidebar vibrancy belongs to the List, so anything outside it shows
            // the window backdrop instead and reads as the wrong material.
            List(SettingsPanel.Page.allCases, selection: $selectedPage) { page in
                Label(page.localizedTitle, systemImage: page.symbolName)
                    .tag(page)
            }
            .listStyle(.sidebar)
            .safeAreaInset(edge: .bottom, spacing: 0) { sidebarFooter }
            .frame(width: 170)
            // The columns sit inside the title-bar safe area, so a plain Divider
            // stops ~32pt short of the top edge and reads as a broken seam.
            Divider().ignoresSafeArea(edges: .top)
            ScrollViewReader { proxy in
                ScrollView {
                    // Zero-height marker at the top of the page, so switching
                    // pages can scroll back to it; a ScrollView keeps its
                    // offset when only its content changes.
                    VStack(spacing: 0) {
                        Color.clear.frame(height: 0).id(Self.pageTop)
                        SettingsPanel(
                            page: selectedPage,
                            agentUsage: model.agentUsage,
                            modelReport: model.modelReport,
                            presentClients: model.stats?.presentClients,
                            isLoading: isInitialLoad,
                            reportLoading: model.modelLoading)
                            .padding(14)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(OverlayScrollerEnforcer())
                    }
                }
                .scrollIndicators(.never)
                .frame(width: 354)
                .onChange(of: selectedPage) {
                    proxy.scrollTo(Self.pageTop, anchor: .top)
                }
                .onAppear {
                    guard let destination else { return }
                    selectedPage = destination.page
                    // A nil anchor means the page itself is the destination
                    // (usage attribution has one section, at the top) — there
                    // is nothing further to scroll to.
                    guard let anchor = destination.anchor else { return }
                    // Next runloop turn: the page's own sections have to exist
                    // before the anchor can be resolved, and selecting the page
                    // above is what creates them.
                    DispatchQueue.main.async {
                        withAnimation(.easeOut(duration: 0.25)) {
                            proxy.scrollTo(anchor, anchor: .top)
                        }
                    }
                }
            }
            Divider().ignoresSafeArea(edges: .top)
            ScrollView {
                previewColumn
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(OverlayScrollerEnforcer())
            }
            .scrollIndicators(.never)
            .frame(width: 330)
        }
        .frame(width: Self.contentSize.width, height: Self.contentSize.height)
        .background(PopoverBackdrop().ignoresSafeArea())
        .task { await model.load() }
        // The attribution page is built from the model report, and `load()` is
        // graph-only since the report came off the critical path. Without this
        // Settings never asks for one: opened cold — `--settings`, or before the
        // popover has ever been shown — the page waits for a request nobody
        // makes, and once `isLoading` clears it reads as permanently
        // unavailable. Keyed like PopoverView's, on the committed slice rather
        // than the payload generation, so an all-years and a current-year view
        // dated the same day still re-fire.
        .task(id: model.committedSliceKey) {
            await model.ensureModelData(for: .stats)
        }
        // See `PopoverView`: keyed so removing an account takes its card away
        // in the same turn rather than at the poll's convenience.
        .task(id: extraRootsGeneration) { await model.pollAgentUsage() }
        .task(id: quotaReconciliationID) {
            guard let payload = model.agentUsage else { return }
            let defaults = UsageDataSources.current.allowsQuotaCachePersistence
                ? UserDefaults.standard
                : nil
            _ = Self.applyQuotaRemaining(
                payload: payload,
                persistedSelection: quotaSourceRaw,
                excluding: ClientRegistry.quotaExcludedClients(),
                defaults: defaults)
        }
        .task { await model.pollTrace() }
        .task { await model.pollGraph() }
        // Key the rate poll on the hidden raw so a hide toggle restarts it and
        // re-fetches the filtered rate immediately, instead of lagging ≤10s.
        .task(id: tabsHiddenRaw) { await pollTokensPerMin() }
        // The preview card's Chart layout needs quota curves, and nothing here
        // was asking for them — so picking Chart changed the radio button and
        // nothing else, which reads as a broken setting rather than a setting
        // whose data is absent.
        //
        // Curves only. `windowUsageClient` stays nil deliberately: that is what
        // gates the message scan, and a Settings window must not start a
        // multi-second scan to render a thumbnail. Each curve is a ~2ms read of
        // an already-persisted file.
        //
        // The identity carries the client list because the closure reads it and
        // it arrives with the graph, after this view first appears.
        // Configured, not assigned. Setting `windowCardClients` directly used
        // the usage-derived list, so a quota-only provider — one with a login
        // but no local records — was never in it and its persisted curve was
        // never read, while the preview card still drew the row from the quota
        // payload. The row appeared with a plain bar instead of its chart.
        // `configureQuotaVisibility` hands the model the same three inputs the
        // popover gives it, and `refreshWindowQuotaHalves` derives membership
        // from them plus the quota publication, which is also what keeps it
        // current as later publications arrive.
        .task(id: "\(tabsHiddenRaw)|\(tabsOrderRaw)|\(limitsHiddenRaw)|"
              + previewClients.joined(separator: ",")) {
            model.configureQuotaVisibility(
                tabHidden: ClientRegistry.parseIdSet(tabsHiddenRaw),
                limitsHidden: ClientRegistry.parseIdSet(limitsHiddenRaw),
                orderRaw: tabsOrderRaw)
            model.refreshWindowQuotaHalves()
        }
    }

    /// Clients the preview card renders, derived exactly as the card itself
    /// derives them.
    ///
    /// This is no longer also the curve set: quota-only providers reach the
    /// card through the quota payload rather than through local usage, so the
    /// curves are derived by `refreshWindowQuotaHalves` from the visibility
    /// configured above. Keeping this list as the curve set is what left those
    /// providers charted as plain bars.
    private var previewClients: [String] {
        ClientRegistry.displayClients(
            present: model.stats?.presentClients ?? [],
            hiddenRaw: tabsHiddenRaw, orderRaw: tabsOrderRaw)
    }

    /// Whether either initial request is still in flight. Keyed on request
    /// lifecycle, NOT on payload presence: `stats`/`agentUsage` stay nil when a
    /// fetch fails (`load()` leaves `stats` nil on error and `pollAgentUsage()`
    /// only assigns on success), so waiting for a payload would spin forever
    /// against a persistent failure instead of reaching a terminal state.
    private var isInitialLoad: Bool {
        Self.isInitialLoad(
            phase: model.phase, agentUsageAttempted: model.agentUsageAttempted)
    }

    nonisolated static func isInitialLoad(
        phase: DashboardModel.Phase, agentUsageAttempted: Bool
    ) -> Bool {
        if case .loading = phase { return true }
        return !agentUsageAttempted
    }

    /// Fills the empty bottom of the sidebar: app icon, name, running version
    /// (the first thing an issue report needs), and the two outbound links. Name
    /// and version come from the bundle, so a rename does not leave a stale
    /// brand here. `Link` opens the default browser without an NSWorkspace detour.
    private var sidebarFooter: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 8) {
                BrandMark()
                    .frame(width: 26, height: 26)
                VStack(alignment: .leading, spacing: 0) {
                    Text(AppInfo.name)
                        .font(.caption.weight(.medium))
                    Text(AppInfo.version)
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.leading, 6)
            HStack(spacing: 4) {
                FooterLink(
                    title: "GitHub",
                    systemImage: "chevron.left.forwardslash.chevron.right",
                    url: "https://github.com/Nanako0129/syrtis")
                FooterLink(
                    title: "Sponsor",
                    systemImage: "heart",
                    url: "https://www.patreon.com/cw/Nanako0129/membership")
            }
        }
        .padding(.horizontal, 6)
        .padding(.bottom, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    nonisolated static func applyQuotaRemaining(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        defaults: UserDefaults?
    ) -> Double? {
        let cachedRemaining = defaults?.object(forKey: TrayAnimator.lastRemainingKey) as? Double
        return TrayAnimator.applyQuotaRemaining(
            payload: payload,
            persistedSelection: persistedSelection,
            excluding: excluding,
            cachedRemaining: cachedRemaining,
            defaults: defaults)
    }

    // MARK: - Preview column

    private var previewColumn: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Live preview — settings apply immediately.")
                .font(.caption2)
                .foregroundStyle(.tertiary)

            section("Menu bar") {
                // The stale rule reads the clock, so with polls failing and
                // the payload unchanged nothing else would redraw the preview
                // when a reading crosses the 30-minute threshold (#420).
                TimelineView(.periodic(from: .now, by: 60)) { _ in
                    VStack(spacing: 6) {
                        MenuBarMock(
                            dark: true, graph: model.payload,
                            tokensPerMin: tokensPerMin, agentUsage: model.agentUsage)
                        MenuBarMock(
                            dark: false, graph: model.payload,
                            tokensPerMin: tokensPerMin, agentUsage: model.agentUsage)
                    }
                }
            }

            if limitsEnabled {
                section("Agent limits card") {
                    AgentLimitsCard(
                        clients: previewClients,
                        trace: model.trace, agentUsage: model.agentUsage,
                        reorderable: true, curves: model.windowCurves)
                }
            }

            section("Live session card") {
                UsageTraceCard(buckets: model.trace, windowSecs: 600, hidden: hiddenClients)
            }
        }
    }

    private func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.localized.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
            content()
        }
    }

    /// Same cadence as the popover: the staticlib tail re-parses at most
    /// every 10s. Feeds the rate tray mode and the preview's spin speed.
    private func pollTokensPerMin() async {
        while !Task.isCancelled {
            let rate = await model.tokensPerMin()
            if Task.isCancelled { break }
            tokensPerMin = rate
            try? await Task.sleep(for: .seconds(10))
        }
    }
}

/// One mock menu-bar strip (dark or light) with the Syrtis status item
/// rendered from the same inputs the real one uses: TrayIcons gauges or the
/// cat/parrot frame sets, plus TrayMode's title over live data.
private struct MenuBarMock: View {
    let dark: Bool
    let graph: UsagePayload?
    let tokensPerMin: Double?
    let agentUsage: AgentUsagePayload?

    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(AnimationPace.storageKey) private var paceRaw = AnimationPace.default.rawValue
    @AppStorage(MenuBarTextColor.storageKey) private var textColorMode = MenuBarTextColor.automatic.rawValue
    @AppStorage(MenuBarTextColor.customColorKey) private var textColorHex = MenuBarTextColor.defaultHex
    @AppStorage(MenuBarTextColor.warningColorKey) private var warningTextColorHex = QuotaColorLevel.warning.defaultHex
    @AppStorage(MenuBarTextColor.criticalColorKey) private var criticalTextColorHex = QuotaColorLevel.critical.defaultHex
    @AppStorage(TrayAnimator.styleKey) private var animationStyle = "cat"
    @AppStorage(TrayAnimator.animateKey) private var animateTray = true
    @AppStorage(IconColoring.storageKey) private var iconColoringRaw = IconColoring.warningOnly.rawValue
    @AppStorage(TrayAnimator.windowLetterKey) private var showsWindowLetter = true
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto

    var body: some View {
        let mode = TrayMode(rawValue: trayModeRaw) ?? .todayTokens
        let remaining = quotaRemaining
        let title = mode.title(
            graph: graph, tokensPerMin: tokensPerMin, quotaRemaining: remaining)
        let level = (mode == .quotaLeft ? remaining : nil)
            .map(QuotaColorLevel.init(remaining:)) ?? .normal
        let customHex: String = switch level {
        case .normal: textColorHex
        case .warning: warningTextColorHex
        case .critical: criticalTextColorHex
        }
        let ink: Color = dark ? .white : .black

        HStack(spacing: 10) {
            Text((dark ? "Dark" : "Light").localized)
                .font(.caption2)
                .foregroundStyle(ink.opacity(0.4))
            Spacer()
            // The Syrtis status item, hover-highlighted to stand out.
            HStack(spacing: title.isEmpty ? 0 : 4) {
                icon(remaining: remaining)
                if !title.isEmpty {
                    Text(title)
                        .font(.system(size: 12).monospacedDigit())
                        .foregroundStyle(
                            MenuBarTextColor.applyingStale(
                                MenuBarTextColor.resolve(
                                    automatic: mode.titleColor(quotaRemaining: remaining),
                                    modeRaw: textColorMode, hex: customHex),
                                // Same stale rule as the menu bar and the icon
                                // beside this title (#420).
                                stale: mode == .quotaLeft && TrayAnimator.readingIsStale(
                                    payload: agentUsage,
                                    persistedSelection: quotaSource,
                                    excluding: ClientRegistry.quotaExcludedClients(),
                                    defaults: UsageDataSources.current.allowsQuotaCachePersistence
                                        ? .standard : nil,
                                    now: Date()),
                                quotaRemaining: mode == .quotaLeft ? remaining : nil,
                                dark: dark)
                                .map(Color.init(nsColor:)) ?? ink)
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(
                (dark ? Color.white : Color.black).opacity(0.16),
                in: RoundedRectangle(cornerRadius: 5))
            Image(systemName: "wifi")
                .font(.system(size: 11))
                .foregroundStyle(ink.opacity(0.5))
            Text(Self.clock)
                .font(.system(size: 12))
                .foregroundStyle(ink.opacity(0.5))
        }
        .padding(.horizontal, 10)
        .frame(height: 27)
        .background(
            dark ? Color(white: 0.13) : Color(white: 0.93),
            in: RoundedRectangle(cornerRadius: 7))
        .overlay(
            RoundedRectangle(cornerRadius: 7)
                .strokeBorder(Color.primary.opacity(0.12), lineWidth: 1))
    }

    /// Mirrors TrayAnimator.quotaRemaining through the shared payload-aware
    /// policy. Excludes the tab- and limits-hidden clients from the auto pick,
    /// same as the real tray.
    private var quotaRemaining: Double? {
        let source = UsageDataSources.current
        let cachedRemaining: Double? = source.allowsQuotaCachePersistence
            ? UserDefaults.standard.object(forKey: TrayAnimator.lastRemainingKey) as? Double
            : nil
        return QuotaSelectionPolicy.resolveRemainingPercent(
            payload: agentUsage,
            persistedSelection: quotaSource,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: cachedRemaining)
    }

    @ViewBuilder
    private func icon(remaining: Double?) -> some View {
        if let gauge = QuotaIconStyle(rawValue: animationStyle) {
            let coloring = IconColoring(rawValue: iconColoringRaw) ?? .warningOnly
            // Same stale rule as the menu bar (#8), evaluated at render time.
            let stale = TrayAnimator.readingIsStale(
                payload: agentUsage,
                persistedSelection: quotaSource,
                excluding: ClientRegistry.quotaExcludedClients(),
                defaults: UsageDataSources.current.allowsQuotaCachePersistence ? .standard : nil,
                now: Date())
            Image(nsImage: TrayIcons.image(
                style: gauge, remaining: remaining, dark: dark, coloring: coloring,
                stale: stale,
                prefix: showsWindowLetter
                    ? QuotaSelectionPolicy.resolveWindowLetter(
                        payload: agentUsage,
                        persistedSelection: quotaSource,
                        excluding: ClientRegistry.quotaExcludedClients())
                    : nil))
        } else {
            let frames = PreviewFrames.frames(
                style: animationStyle, dark: dark,
                level: TrayAnimator.sandLevel(
                    tokensPerMinute: (AnimationPace(rawValue: paceRaw) ?? .default)
                        .scaled(tokensPerMin ?? 0)))
            if frames.isEmpty {
                Image(systemName: "chart.bar.fill")
                    .font(.system(size: 12))
            } else if animateTray {
                let interval = frameInterval
                TimelineView(.periodic(from: .now, by: interval)) { timeline in
                    let index = Int(
                        timeline.date.timeIntervalSinceReferenceDate / interval)
                        % frames.count
                    Image(nsImage: frames[index])
                }
            } else {
                Image(nsImage: frames[0])
            }
        }
    }

    /// Same pacing as TrayAnimator: idle 2 fps up to 50k tok/min, a log ramp
    /// to 40 fps at 3M.
    private var frameInterval: TimeInterval {
        if animationStyle == TrayAnimator.sandStyle { return 1 / TrayAnimator.sandFPS }
        let pace = AnimationPace(rawValue: paceRaw) ?? .default
        return Double(TrayAnimator.animationIntervalMilliseconds(
            load: TrayAnimator.animationLoad(tokensPerMinute: pace.scaled(tokensPerMin ?? 0)))) / 1000
    }

    private static let clock = Date.now.formatted(date: .omitted, time: .shortened)
}

/// Cat/parrot frame sets for the mock strips, loaded once per
/// (style, appearance) from the same bundle directories TrayAnimator uses.
@MainActor
private enum PreviewFrames {
    private static var cache: [String: [NSImage]] = [:]

    static func frames(style: String, dark: Bool, level: Int = 0) -> [NSImage] {
        let base: String
        switch style {
        case "parrot": base = "anim-parrot"
        case TrayAnimator.sandStyle: base = "anim-sand\(level)"
        default: base = "anim-cat2"
        }
        let directory = base + (dark ? "" : "-light")
        if let hit = cache[directory] { return hit }
        let loaded = TrayAnimator.loadFrames(directory: directory)
        cache[directory] = loaded
        return loaded
    }
}
