import AppKit
import SwiftUI
import TokenBarCore

/// In-popover settings, port of SettingsPanel.tsx. Every control binds the
/// same UserDefaults keys the cards/tray read live. Autostart, tray animation
/// and the updater arrive with their subsystems in later phases.
struct SettingsPanel: View {
    enum Page: String, CaseIterable, Identifiable {
        case menuBar = "Menu bar"
        case dashboard = "Dashboard"
        case usageAttribution = "Usage attribution"
        case general = "General"
        case about = "About"

        var id: Self { self }

        var localizedTitle: String { rawValue.localized }

        var symbolName: String {
            switch self {
            case .menuBar: "menubar.rectangle"
            case .dashboard: "chart.bar.xaxis"
            case .usageAttribution: "arrow.triangle.branch"
            case .general: "gearshape"
            case .about: "info.circle"
            }
        }
    }

    var page: Page = .menuBar

    /// For the quota-source picker (the windows currently known).
    var agentUsage: AgentUsagePayload?

    /// Settings receives the all-time report so every observed provider stays
    /// configurable even when the dashboard itself is scoped to one year.
    var modelReport: ModelReport?

    /// Present clients (used for the client tabs reorder/hide UI).
    /// nil until an accepted payload lands, which is DIFFERENT from a scan
    /// that found nothing. Only the Discord picker distinguishes them; the
    /// other readers below want "whatever is present right now" and flatten it.
    var presentClients: [String]?

    /// True while either initial request is still in flight. An empty client list
    /// is otherwise indistinguishable from "still loading", and the first seconds
    /// after opening Settings would claim there are no eligible clients. The
    /// caller derives this from request lifecycle, not payload presence — a failed
    /// fetch leaves the payload nil forever.
    var isLoading = false
    /// True while the model-report request is in flight. Separate from
    /// `isLoading`, which tracks the graph and quota requests: the report has
    /// its own lifecycle since it came off the critical path, and folding it
    /// into the same flag would call the page unavailable while it is still
    /// being fetched.
    var reportLoading = false

    /// The popover's onboarding card also writes confirmed attribution. The
    /// page reads its tables through `attributionRevision`, which only this
    /// view bumps, so an outside write is observed here and bumps it too.
    @AppStorage(UsageAttribution.confirmedKey) private var observedConfirmedRaw = ""
    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(MenuBarTextColor.storageKey) private var textColorMode = MenuBarTextColor.automatic.rawValue
    @AppStorage(MenuBarTextColor.customColorKey) private var textColorHex = MenuBarTextColor.defaultHex
    @AppStorage(MenuBarTextColor.warningColorKey) private var warningTextColorHex = QuotaColorLevel.warning.defaultHex
    @AppStorage(MenuBarTextColor.criticalColorKey) private var criticalTextColorHex = QuotaColorLevel.critical.defaultHex
    @State private var editingTextColor: QuotaColorLevel?
    @AppStorage(TrayAnimator.animateKey) private var animateTray = true
    @AppStorage(TrayAnimator.styleKey) private var animationStyle = "cat"
    @AppStorage(AnimationPace.storageKey) private var animationPaceRaw = AnimationPace.default.rawValue
    @AppStorage(IconColoring.storageKey) private var iconColoringRaw = IconColoring.warningOnly.rawValue
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto
    @AppStorage(ClientTray.enabledKey) private var individualEnabledRaw = ""
    @AppStorage(ClientTray.selectionsKey) private var individualSelectionsRaw = "{}"
    @AppStorage("tokenbar.updates.beta") private var betaUpdates = false
    /// Loaded once per panel appearance without blocking SwiftUI body creation.
    @State private var autostartEnabled = false
    /// Set only after the service accepts a user mutation, so a slower initial
    /// read cannot overwrite committed state while a failed mutation still lets
    /// that authoritative read settle the switch.
    @State private var autostartMutationCommitted = false
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    @AppStorage("tokenbar.views.hidden") private var hiddenViewsRaw = ""
    @AppStorage(OverviewCard.hiddenKey) private var overviewHiddenRaw = ""
    @AppStorage("tokenbar.limits.asUsed") private var limitsAsUsed = false
    @AppStorage("tokenbar.limits.paceMode") private var paceModeRaw = PaceMode.historical.rawValue
    @AppStorage("tokenbar.limits.layout") private var layoutRaw = LimitsLayout.full.rawValue
    @AppStorage("tokenbar.trace.detailed") private var detailedTrace = false
    @AppStorage("tokenbar.refresh.intervalMin") private var refreshIntervalMin = 30
    @AppStorage(AppLanguage.storageKey) private var languageRaw = AppLanguage.system.rawValue
    /// The ONLY binding of this key in the app, and the only place other than
    /// `DiscordPresence.enabled()` where its default appears. A second
    /// declaration that said `true` would read as "already on" after an
    /// upgrade — this repo has precedent (`tokenbar.limits.enabled` declares
    /// its default in two views).
    @AppStorage(DiscordPresence.enabledKey) private var discordEnabled = false
    @AppStorage(DiscordPresence.wholeDollarsKey) private var discordWholeDollars = false
    /// Stored as the raw comma-separated string the payload layer parses, so
    /// the view and `DiscordPresence.components()` cannot drift into two
    /// different ideas of what is selected.
    @AppStorage(DiscordPresence.componentsKey) private var discordComponentsRaw =
        DiscordPresence.defaultComponentsRaw
    /// Empty means the busiest visible client. Stored as the raw id so the
    /// panel and `DiscordPresence.selection()` read one value.
    @AppStorage(DiscordPresence.selectionKey) private var discordSelectionRaw = ""
    @State private var showLanguageRestartPrompt = false
    @State private var attributionNotice: String?
    @State private var attributionRevision = 0
    /// Loaded once per panel appearance; mutations write through
    /// `ClaudeExtraRoots.save`/`.apply` immediately (D1: no restart).
    @State private var claudeExtraRoots: [String] = ClaudeExtraRoots.load()
    @State private var claudeExtraRootsResult: ExtraScanPathsResult?
    /// Filled off the main actor — see `ClaudeExtraRoots.missingRoots`. Empty
    /// until the first probe lands, so a row shows no warning rather than a
    /// wrong one while the answer is still unknown.
    @State private var missingClaudeRoots: Set<String> = []
    /// The stored list itself, observed rather than copied: automatic capture
    /// changes it while this panel is open, and a copy held here would be
    /// written back over that change.
    @AppStorage(AntigravityAccounts.storageKey) private var antigravityAccountsRaw = ""
    @AppStorage(AntigravityAutoCapture.enabledKey) private var antigravityAutoCaptureOn = false
    /// Busy, paused, unavailable and the last failure sentence.
    @ObservedObject private var antigravityAutoCapture = AntigravityAutoCapture.shared
    @ObservedObject private var cursorSync = CursorSyncController.shared
    @AppStorage(CursorSync.enabledKey) private var cursorSyncOn = true
    @AppStorage(CursorSync.takeoverKey) private var cursorTakeover = false
    @AppStorage(CursorSync.noticeKey) private var cursorNoticeAck = false
    /// 0 = auto (≈60% of the screen). The popover's drag handle writes the
    /// same key, so the two stay in sync.
    @AppStorage(PopoverChrome.heightKey) private var popoverHeight = 0.0

    // New for tabs improvement
    @AppStorage(ClientRegistry.tabOrderKey) private var tabsOrderRaw = ""
    @AppStorage(ClientRegistry.tabHiddenKey) private var tabsHiddenRaw = ""
    /// Per-client Agent-limits visibility, independent of tab visibility.
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""

    // Drag state for client tabs reorder (scoped to this panel)
    @State private var tabsDragId: String?
    @State private var tabsOverId: String?
    @State private var tabsCardFrames: [String: CGRect] = [:]

    private static let tabsDragSpace = "client-tabs-order"

    private struct TabsCardFramesKey: PreferenceKey {
        static let defaultValue: [String: CGRect] = [:]
        static func reduce(value: inout [String: CGRect], nextValue: () -> [String: CGRect]) {
            value.merge(nextValue(), uniquingKeysWith: { $1 })
        }
    }

    static let refreshIntervalOptions = [1, 5, 15, 30, 60]

    /// First-wins dedup of two id lists, preserving order (a's entries first,
    /// then b's not already seen). Used to build the client-tabs universe from
    /// present clients ∪ quota-card clients.
    private static func orderedUnion(_ a: [String], _ b: [String]) -> [String] {
        var seen = Set<String>()
        return (a + b).filter { seen.insert($0).inserted }
    }

    // MARK: - Client tabs drag reorder helpers (adapted from AgentLimitsCard)

    private func dropEdge(for id: String, in orderList: [String]) -> VerticalEdge? {
        guard let dragId = tabsDragId,
              tabsOverId == id,
              dragId != id,
              let fromI = orderList.firstIndex(of: dragId),
              let toI = orderList.firstIndex(of: id)
        else { return nil }
        return fromI < toI ? .bottom : .top
    }

    private func dragGestureForTab(id: String, orderList: [String]) -> some Gesture {
        DragGesture(minimumDistance: 2, coordinateSpace: .named(Self.tabsDragSpace))
            .onChanged { value in
                tabsDragId = id
                let over = tabsCardFrames.first { $0.value.contains(value.location) }?.key
                tabsOverId = (over != nil && over != id) ? over : nil
            }
            .onEnded { _ in
                if let over = tabsOverId, over != id {
                    let next = ClientRegistry.reorder(orderList, from: id, to: over)
                    tabsOrderRaw = next.joined(separator: ",")
                }
                tabsDragId = nil
                tabsOverId = nil
            }
    }

    var body: some View {
        // Computed once and shared by the two sections below (Agent limits +
        // Client tabs), instead of re-deriving `knownClientIds` per section.
        // `orderRaw:` overloads keep both lists reactive to a drag/reorder.
        let knownIds = AgentLimitsCard.knownClientIds(
            agentUsage: agentUsage, present: presentClients ?? [])
        // Agent-limits management universe: only clients that can actually
        // render a quota card (placeholder rows or a live snapshot).
        let limitOrdered = ClientRegistry.orderedClients(knownIds, orderRaw: tabsOrderRaw)
        // Client-tabs universe: every client that can be a top tab (present)
        // OR a quota card (knownIds — e.g. quota-only Antigravity), so both
        // orderings are managed from one list. Mirrors displayClients' source.
        let tabClients = ClientRegistry.tabClients(
            present: presentClients ?? [], quotaIds: agentUsage?.configuredClientIds ?? [])
        let presentSet = Set(tabClients)
        let tabsUniverse = ClientRegistry.orderedClients(
            Self.orderedUnion(tabClients, knownIds), orderRaw: tabsOrderRaw)

        VStack(alignment: .leading, spacing: 14) {
            switch page {
            case .menuBar:
                menuBarPage()
            case .dashboard:
                dashboardPage(
                    limitOrdered: limitOrdered,
                    presentSet: presentSet,
                    tabsUniverse: tabsUniverse)
            case .usageAttribution:
                usageAttributionPage()
            case .general:
                generalPage()
            case .about:
                aboutPage()
            }
        }
        .task {
            guard AutostartService.isAvailable else { return }
            let enabled = await AutostartService.readEnabled()
            // The query takes ~0.5-0.9s. If the user flipped the switch while it
            // was in flight, `setEnabled` has already changed the service and
            // this result is stale — applying it would leave the control showing
            // the opposite of the real state until Settings is reopened.
            guard !Task.isCancelled, !autostartMutationCommitted else { return }
            autostartEnabled = enabled
        }
        .alert("Restart Syrtis?", isPresented: $showLanguageRestartPrompt) {
            Button("Later", role: .cancel) {}
            Button("Restart Now") { AppRelauncher.relaunch() }
        } message: {
            Text("Restart Syrtis to apply the new language.")
        }
        .task(id: attributionInputSignature) {
            refreshAttributionSuggestions()
        }
        .onChange(of: observedConfirmedRaw) { attributionRevision += 1 }
    }

    private var attributionTables: (
        confirmed: UsageAttribution.Table, suggestions: UsageAttribution.Table
    ) {
        _ = attributionRevision
        return (UsageAttribution.confirmed(), UsageAttribution.suggestions())
    }

    private var attributionTargetClients: [String] {
        UsageAttributionSettings.subscriptionClients(from: agentUsage)
    }

    /// Identity of the inputs a suggestion refresh depends on. The target list
    /// has to contribute its *knownness*, not just its contents: a payload that
    /// arrives carrying zero subscription candidates leaves
    /// `attributionTargetClients` empty, which is the same string it was while
    /// the request was still in flight. Without the lifecycle token the task
    /// never reruns, so the page stops suppressing stored suggestions without
    /// ever reconciling them against the now-known empty set.
    private var attributionInputSignature: String? {
        guard let modelReport else { return nil }
        return UsageAttributionSettings.signature(
            entries: modelReport.entries,
            subscriptionClients: attributionTargetClients,
            targetsKnown: agentUsage != nil,
            routedSubscriptions: UsageAttributionSettings.routedSubscriptions(from: agentUsage))
    }

    @ViewBuilder
    private func menuBarPage() -> some View {
        section("Menubar title") {
            radioGroup(
                selection: $trayModeRaw,
                options: TrayMode.allCases.map { ($0.rawValue, $0.label) })
        }

        section("Font color") {
            radioGroup(
                selection: $textColorMode,
                options: MenuBarTextColor.allCases.map { ($0.rawValue, $0.label) })
            if textColorMode == MenuBarTextColor.custom.rawValue {
                row("Custom color") {
                    HStack(spacing: 14) {
                        ForEach(QuotaColorLevel.allCases, id: \.self) { level in
                            MenuBarTextColorControl(
                                level: level, hex: textColorBinding(level), editingLevel: $editingTextColor)
                                .popover(
                                    isPresented: textColorEditorPresented(level), arrowEdge: .bottom
                                ) {
                                    MenuBarTextColorPopover(
                                        hex: textColorBinding(level), level: level)
                                        .id(level)
                                }
                        }
                    }
                }
            }
            hint("Custom colors follow remaining quota: normal above 25%, low at 25% or less, very low at 10% or less. A stale reading always turns grey.")
        }

        section("Menubar icon") {
            radioGroup(
                selection: $animationStyle,
                options: TrayAnimator.iconStyleOptions)
            if isAnimatedStyle {
                toggleRow("Animate based on token usage", isOn: $animateTray)
                if animateTray {
                    radioGroup(
                        selection: $animationPaceRaw,
                        options: AnimationPace.allCases.map { ($0.rawValue, $0.label) })
                    hint((AnimationPace(rawValue: animationPaceRaw) ?? .default).detail)
                    hint("The cat and parrot speed up and the sand gets busier as the live token rate climbs. The pace sets how much traffic reaches the top: Light at 600K tokens/min, Moderate at 3M, Heavy at 10M.")
                }
            } else {
                radioGroup(
                    selection: $iconColoringRaw,
                    options: IconColoring.allCases.map { ($0.rawValue, $0.label) })
                hint("Gauge icons empty as quota is used. \"Color on warning only\" keeps one color until 25% is left, then amber, and red at 10%.")
            }
        }

        section("Quota source") {
            quotaSourcePicker()
            hint("Which quota the gauge icon and the \"Quota left\" title follow. Auto follows whichever window, across all your agents, has the least left.")
        }

        individualItemsSection()
    }

    @ViewBuilder
    private func individualItemsSection() -> some View {
        let rows = ClientTray.settingsRows(
            presentClients: presentClients ?? [],
            payload: agentUsage,
            enabled: ClientTray.parseEnabledRaw(individualEnabledRaw),
            selections: ClientTray.parseSelectionsRaw(individualSelectionsRaw),
            hidden: ClientRegistry.parseIdSet(tabsHiddenRaw),
            orderRaw: tabsOrderRaw,
            officialClients: AgentIconView.availableOfficialClientIDs())

        section("Individual items") {
            hint("Add a separate menu bar item for a client, showing one of its quota windows. Auto shows the one with the least left.")
            if rows.isEmpty, isLoading {
                LoadingLine(title: "Looking for eligible clients…")
            } else if rows.isEmpty {
                Text("No clients with quota yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 1) {
                    ForEach(rows) { row in
                        individualItemRow(row)
                    }
                }
                .glassCard(cornerRadius: 8)
            }
        }
    }

    private func textColorBinding(_ level: QuotaColorLevel) -> Binding<String> {
        switch level {
        case .normal: $textColorHex
        case .warning: $warningTextColorHex
        case .critical: $criticalTextColorHex
        }
    }

    private func textColorEditorPresented(_ level: QuotaColorLevel) -> Binding<Bool> {
        Binding(
            get: { editingTextColor == level },
            set: { presented in
                if !presented, editingTextColor == level {
                    editingTextColor = nil
                }
            })
    }

    private func individualItemRow(_ row: ClientTray.SettingsRow) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 7) {
                AgentIconView(clientId: row.clientId, size: 16)
                    .accessibilityHidden(true)
                Text(row.displayName)
                    .font(.caption)
                    .accessibilityHidden(true)
                Spacer()
                Text(row.valueText)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .accessibilityLabel(row.accessibilityLabel)
                Toggle("", isOn: Binding(
                    get: { row.isEnabled },
                    set: { next in
                        if let raw = ClientTray.enabledRaw(
                            updating: individualEnabledRaw,
                            clientId: row.clientId,
                            enabled: next)
                        {
                            individualEnabledRaw = raw
                        }
                    }))
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .labelsHidden()
                    .accessibilityLabel("Show %@ individual item".localized(row.displayName))
            }

            if row.isEnabled {
                HStack {
                    Text("Window")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Picker("", selection: Binding(
                        get: { row.selection },
                        set: { next in
                            if let raw = ClientTray.selectionsRaw(
                                updating: individualSelectionsRaw,
                                clientId: row.clientId,
                                selection: next)
                            {
                                individualSelectionsRaw = raw
                            }
                        })) {
                        ForEach(row.options) { option in
                            Text(option.label.localized)
                                .tag(option.tag)
                                .disabled(!option.isEnabled)
                        }
                    }
                    .labelsHidden()
                    .accessibilityLabel("Quota window for %@".localized(row.displayName))
                    .pickerStyle(.menu)
                    .frame(maxWidth: 210)
                }
            }

            if let statusHint = row.statusHint {
                Text(statusHint.localized)
                    .font(.caption2)
                    .foregroundStyle(row.status == .errorExplicit ? .secondary : .tertiary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
    }

    @ViewBuilder
    private func dashboardPage(
        limitOrdered: [String],
        presentSet: Set<String>,
        tabsUniverse: [String]
    ) -> some View {
        section("Agent limits") {
            toggleRow("Show Agent limits card", isOn: $limitsEnabled)
            hint("Off hides the quota card on Overview and on every client tab.")

            if limitsEnabled {
                toggleRow("Show as used", isOn: $limitsAsUsed)
                hint("On: bars show how much you've used. Off: bars show what's left.")
                radioGroup(
                    selection: $layoutRaw,
                    options: LimitsLayout.allCases.map { ($0.rawValue, "Layout: \($0.rawValue.capitalized)") })
                hint("Full: bar with a pace marker. Classic: compact bar, no pace. Chart: quota over time, with pace as a second line; windows without history show a bar.")
                if LimitsLayout(rawValue: layoutRaw) != .classic {
                    radioGroup(
                        selection: $paceModeRaw,
                        options: PaceMode.allCases.map { ($0.rawValue, "Pace: \($0.rawValue.capitalized)") })
                    hint("Shows whether you're using quota faster or slower than expected. Historical compares with your past windows and uses Linear until it has enough history. Linear assumes even use until the reset.")
                }

                if !limitOrdered.isEmpty {
                    let limitsHiddenSet = ClientRegistry.parseIdSet(limitsHiddenRaw)
                    // A tab hidden below always hides its quota card too — the
                    // toggle here reflects that (off + disabled) rather than
                    // offering a state the card can never actually reach.
                    let tabHiddenSet = ClientRegistry.withGroupMembers(ClientRegistry.parseIdSet(tabsHiddenRaw))
                    Divider()
                    VStack(spacing: 1) {
                        ForEach(limitOrdered, id: \.self) { id in
                            let tabHidden = tabHiddenSet.contains(id)
                            HStack {
                                HStack(spacing: 6) {
                                    AgentIconView(clientId: id, size: 14)
                                    Text(ClientRegistry.shortName(id))
                                        .font(.caption)
                                }
                                Spacer()
                                Toggle("", isOn: Binding(
                                    get: { !tabHidden && !limitsHiddenSet.contains(id) },
                                    set: { show in
                                        var hidden = limitsHiddenSet
                                        if show {
                                            hidden.remove(id)
                                        } else {
                                            hidden.insert(id)
                                        }
                                        limitsHiddenRaw = hidden.sorted().joined(separator: ",")
                                    }
                                ))
                                .disabled(tabHidden || DemoData.ignoresLocalVisibility)
                                .toggleStyle(.switch)
                                .controlSize(.mini)
                                .labelsHidden()
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 7)
                            .opacity(tabHidden ? 0.5 : 1)
                        }
                    }
                    .glassCard(cornerRadius: 8)
                    hint("Hide one client's quota card and keep its tab. Grayed out when the tab is hidden, which already hides the card.")
                }
            }
        }

        section("Overview cards") {
            let hiddenCards = ClientRegistry.parseIdSet(overviewHiddenRaw)
            VStack(spacing: 1) {
                ForEach(OverviewCard.toggleable, id: \.self) { card in
                    HStack {
                        Text(card.label.localized)
                            .font(.caption)
                        Spacer()
                        Toggle("", isOn: Binding(
                            get: { !hiddenCards.contains(card.rawValue) },
                            set: { show in
                                var hidden = hiddenCards
                                if show { hidden.remove(card.rawValue) }
                                else { hidden.insert(card.rawValue) }
                                overviewHiddenRaw = hidden.sorted().joined(separator: ",")
                            }
                        ))
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 7)
                }
            }
            .glassCard(cornerRadius: 8)
            hint("Choose which cards Overview shows. The usage chart always stays.")
        }

        section("View tabs") {
            let hiddenViews = ClientRegistry.parseIdSet(hiddenViewsRaw)
            VStack(spacing: 1) {
                ForEach(AppView.toggleable, id: \.self) { view in
                    HStack {
                        Text(view.label)
                            .font(.caption)
                        Spacer()
                        Toggle("", isOn: Binding(
                            get: { !hiddenViews.contains(view.rawValue) },
                            set: { show in
                                var hidden = hiddenViews
                                if show { hidden.remove(view.rawValue) } else { hidden.insert(view.rawValue) }
                                hiddenViewsRaw = hidden.sorted().joined(separator: ",")
                            }
                        ))
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 7)
                }
            }
            .glassCard(cornerRadius: 8)
            hint("Hide views you don't use from the tab row.")
        }

        section("Client tabs (top bar)") {
            let hiddenSet = ClientRegistry.parseIdSet(tabsHiddenRaw)

            if tabsUniverse.isEmpty {
                Text("No clients found yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Drag to order tabs and quota cards. The switch hides a client's tab and its quota card.")
                        .font(.caption2)
                        .foregroundStyle(.secondary)

                    VStack(spacing: 1) {
                        ForEach(tabsUniverse, id: \.self) { id in
                            let isVisible = !hiddenSet.contains(id)
                            // Only present clients can be top tabs, so only
                            // they get the show/hide switch. Quota-only ids
                            // (e.g. Antigravity — OAuth quota, no local
                            // sessions) appear solely to order their quota
                            // card, so they show a caption instead.
                            let canTab = presentSet.contains(id)
                            HStack(spacing: 8) {
                                // Drag handle - always shown for every provider
                                Text("⠿")
                                    .font(.caption)
                                    .foregroundStyle(tabsDragId == id ? .primary : .tertiary)
                                    .help("Drag to reorder")
                                    .gesture(dragGestureForTab(id: id, orderList: tabsUniverse))

                                AgentIconView(clientId: id, size: 14)
                                Text(ClientRegistry.tabLabel(id))
                                    .font(.caption)

                                if !canTab {
                                    Text("(quota card only)")
                                        .font(.caption2)
                                        .foregroundStyle(.tertiary)
                                }

                                Spacer()

                                if canTab {
                                    Toggle("", isOn: Binding(
                                        get: { isVisible },
                                        set: { show in
                                            var hidden = hiddenSet
                                            if show {
                                                hidden.remove(id)
                                            } else {
                                                hidden.insert(id)
                                            }
                                            tabsHiddenRaw = hidden.sorted().joined(separator: ",")
                                        }
                                    ))
                                    .disabled(DemoData.ignoresLocalVisibility)
                                    .toggleStyle(.switch)
                                    .controlSize(.mini)
                                    .labelsHidden()
                                }
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 7)
                            .opacity(tabsDragId == id ? 0.5 : 1)
                            .overlay(alignment: dropEdge(for: id, in: tabsUniverse) == .top ? .top : .bottom) {
                                if let edge = dropEdge(for: id, in: tabsUniverse) {
                                    Rectangle()
                                        .fill(Color.accentColor)
                                        .frame(height: 2)
                                        .offset(y: edge == .top ? -3 : 3)
                                }
                            }
                            .background(
                                GeometryReader { geo in
                                    Color.clear.preference(
                                        key: TabsCardFramesKey.self,
                                        value: [id: geo.frame(in: .named(Self.tabsDragSpace))])
                                })
                        }
                    }
                    .coordinateSpace(name: Self.tabsDragSpace)
                    .onPreferenceChange(TabsCardFramesKey.self) { tabsCardFrames = $0 }
                    .glassCard(cornerRadius: 8)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            hint("Drag to order tabs and quota cards. The switch hides a client's tab and its quota card. Clients that only report quota, such as Antigravity, have no tab and are listed only for ordering.")
        }

        section("Live trace") {
            toggleRow("Split by agent / model", isOn: $detailedTrace)
            hint("On: the live session card gives each agent and model its own row. Off: one row per app.")
        }

        section("Popover size") {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text("Height")
                        .font(.caption)
                    Spacer()
                    Text("\(Int(popoverHeightBinding.wrappedValue.rounded())) pt")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                    if popoverHeight > 0 {
                        Button("Auto") { popoverHeight = 0 }
                            .controlSize(.mini)
                            .buttonStyle(.plain)
                            .font(.caption2)
                            .foregroundStyle(.tint)
                            .help("Fit the height to the screen automatically")
                    }
                }
                Slider(
                    value: popoverHeightBinding,
                    in: Double(PopoverChrome.minHeight)...popoverHeightMax,
                    step: 10)
                    .controlSize(.small)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .glassCard(cornerRadius: 8)
            hint("You can also drag the bottom edge of the popover. Auto uses about 60% of your screen height.")
        }
    }

    @ViewBuilder
    private func usageAttributionPage() -> some View {
        let tables = attributionTables
        let targetClients = attributionTargetClients
        // A stored suggestion proposes a target, and until the quota payload
        // says which subscriptions exist there is nothing to check that target
        // against. Suppressing rather than deleting keeps a valid table intact
        // across a transient failure — the alternative destroys real proposals
        // every time the request happens to be in flight.
        let suggestions = agentUsage == nil ? [] : tables.suggestions.records
        let rows = UsageAttributionSettings.rows(
            entries: modelReport?.entries ?? [],
            confirmed: tables.confirmed.records,
            suggestions: suggestions)
        let suggestedRows = rows.filter { $0.suggestedState != nil }

        section(UsageAttributionSettings.Copy.section) {
            hint(UsageAttributionSettings.Copy.classifyHint)
            hint(UsageAttributionSettings.Copy.canonicalizationHint)

            if let attributionNotice {
                Text(attributionNotice.localized)
                    .font(.caption2)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if !suggestedRows.isEmpty {
                Button {
                    acceptAllAttributionSuggestions()
                } label: {
                    Label(
                        UsageAttributionSettings.Copy.acceptSuggestions.localized(
                            Int64(suggestedRows.count)),
                        systemImage: "checkmark.circle")
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                hint(UsageAttributionSettings.Copy.suggestionsHint)
            }

            switch UsageAttributionSettings.pageState(
                hasReport: modelReport != nil, rowCount: rows.count,
                isLoading: isLoading || reportLoading)
            {
            case .loading:
                LoadingLine(title: "Loading usage…")
            case .unavailable:
                Text(UsageAttributionSettings.Copy.unavailable.localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            case .empty:
                Text(UsageAttributionSettings.Copy.noRows.localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            case .rows:
                VStack(spacing: 1) {
                    ForEach(rows) { row in
                        attributionRow(row, targetClients: targetClients)
                    }
                }
                .glassCard(cornerRadius: 8)
            }
        }
    }

    private func attributionRow(
        _ row: UsageAttributionSettings.Row, targetClients: [String]
    ) -> some View {
        // Every target this row can legitimately hold has to be selectable, and
        // `targetClients` only lists clients with a quota snapshot. Two kinds of
        // target fall outside it: one already confirmed, and one being suggested
        // for a plan Syrtis draws no meter for — a Cursor row is exactly that.
        // Offering the suggestion while the picker cannot select it leaves
        // "Accept all" as the only way to take it, and then undoing everything
        // else it accepted.
        let outOfBandTargets: [String] = [row.state, row.suggestedState]
            .compactMap { state in
                guard case let .assigned(target)? = state else { return nil }
                return target
            }
        var pickerTargets = targetClients
        for target in outOfBandTargets where !pickerTargets.contains(target) {
            pickerTargets.append(target)
        }

        return VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(UsageAttributionSettings.Copy.source.localized(
                        ClientRegistry.style(row.client).displayName,
                        row.providerLabel.localized))
                        .font(.caption.weight(.medium))
                    Text(UsageAttributionSettings.Copy.observed.localized(
                        Format.compactTokens(row.tokens), Format.money(tokens: row.tokens, cost: row.cost)))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
                // The label takes the slack instead of a Spacer so a long
                // source name uses the full width before wrapping, and the
                // picker below keeps a fixed width rather than shrinking to
                // its selected title — otherwise every row's control starts
                // and ends at a different x.
                .frame(maxWidth: .infinity, alignment: .leading)
                Picker(UsageAttributionSettings.Copy.classification.localized, selection: Binding(
                    get: { row.state },
                    set: { next in
                        saveAttribution(row: row, state: next)
                    }))
                {
                    Text(UsageAttributionSettings.Copy.unassigned.localized)
                        .tag(UsageAttribution.State.unassigned)
                    Text(UsageAttributionSettings.Copy.excluded.localized)
                        .tag(UsageAttribution.State.excluded)
                    ForEach(pickerTargets, id: \.self) { target in
                        Text(UsageAttributionSettings.Copy.assigned.localized(
                            ClientRegistry.style(target).displayName))
                            .tag(UsageAttribution.State.assigned(target))
                    }
                }
                .labelsHidden()
                .pickerStyle(.menu)
                .frame(width: 168)
                .accessibilityLabel(UsageAttributionSettings.Copy.classificationFor.localized(
                    row.providerLabel.localized))
            }

            if let suggestedState = row.suggestedState {
                Text(Self.suggestionLabel(for: suggestedState))
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(.orange)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
    }

    /// A suggestion can propose "not a subscription" as readily as a target, so
    /// the label follows the proposed state rather than assuming an assignment.
    static func suggestionLabel(for state: UsageAttribution.State) -> String {
        switch state {
        case let .assigned(target):
            return UsageAttributionSettings.Copy.suggested.localized(
                ClientRegistry.style(target).displayName)
        case .excluded:
            return UsageAttributionSettings.Copy.suggestedExcluded.localized
        case .unassigned:
            return UsageAttributionSettings.Copy.unassigned.localized
        }
    }

    private func saveAttribution(
        row: UsageAttributionSettings.Row, state: UsageAttribution.State
    ) {
        let record = UsageAttribution.Record(
            client: row.client, provider: row.provider, state: state)
        let defaults = UserDefaults.standard
        let table = UsageAttribution.confirmed(defaults: defaults)
        let result = UsageAttribution.confirmedRaw(
            updating: defaults.object(forKey: UsageAttribution.confirmedKey), record: record)
        guard let result else {
            attributionNotice = UsageAttributionSettings.writeFailure(
                table: table, record: record, result: result)?.message
            return
        }
        defaults.set(result, forKey: UsageAttribution.confirmedKey)
        attributionNotice = nil
        attributionRevision += 1
    }

    private func refreshAttributionSuggestions() {
        guard let report = modelReport, agentUsage != nil else { return }
        let defaults = UserDefaults.standard
        let confirmed = UsageAttribution.confirmed(defaults: defaults)
        let targetClients = attributionTargetClients
        let proposed = UsageAttributionSettings.suggestionRecords(
            entries: report.entries,
            confirmed: confirmed.records,
            subscriptionClients: targetClients,
            routedSubscriptions: UsageAttributionSettings.routedSubscriptions(from: agentUsage))
        let table = UsageAttribution.suggestions(defaults: defaults)
        let result = UsageAttribution.suggestionsRaw(
            replacing: defaults.object(forKey: UsageAttribution.suggestionsKey), with: proposed)
        guard let result else {
            attributionNotice = UsageAttributionSettings.writeFailure(
                table: table, records: proposed, result: result)?.message
            attributionRevision += 1
            return
        }
        defaults.set(result, forKey: UsageAttribution.suggestionsKey)
        attributionNotice = nil
        attributionRevision += 1
    }

    private func acceptAllAttributionSuggestions() {
        let tables = attributionTables
        let rows = UsageAttributionSettings.rows(
            entries: modelReport?.entries ?? [],
            confirmed: tables.confirmed.records,
            suggestions: tables.suggestions.records)
        let records = UsageAttributionSettings.acceptanceRecords(rows: rows)
        guard !records.isEmpty else { return }

        // Shared with the onboarding card's "Apply suggestions" button — one
        // write path decides what accepting a suggestion means, so the two
        // surfaces cannot confirm records by different rules.
        attributionNotice = UsageAttributionSettings.accept(records, defaults: .standard)
        attributionRevision += 1
    }

    @ViewBuilder
    private func generalPage() -> some View {
        if AutostartService.isAvailable {
            section("Startup") {
                toggleRow(
                    "Launch at login",
                    isOn: Binding(
                        get: { autostartEnabled },
                        set: { next in
                            if AutostartService.setEnabled(next) {
                                autostartMutationCommitted = true
                                autostartEnabled = next
                            }
                        }))
            }
        }

        section("Data refresh") {
            radioGroup(
                selection: Binding(
                    get: { String(refreshIntervalMin) },
                    set: { refreshIntervalMin = Int($0) ?? 30 }),
                options: Self.refreshIntervalOptions.map {
                    (String($0), $0 == 60 ? "Every hour" : "Every %lld min".localized($0))
                })
            hint("How often Syrtis rereads all logs in full. In between, new activity still shows up within 5 minutes.")
        }

        section("Discord") {
            toggleRow("Show today's usage on Discord", isOn: $discordEnabled)
            // Consent copy, not a feature blurb. It has to say what leaves the
            // machine, who ends up seeing it, and that switching back off does
            // not undo it — the reference implementation ships "Show today's
            // tokens, cost, and most-used AI tool in your Discord activity",
            // which describes the display and hides the disclosure.
            // Names the second switch's consequence here, in the disclosure the
            // user reads BEFORE opting in, rather than only next to the switch
            // itself. Saying "a cost range" while a setting below can turn it
            // into a figure would describe a state the app may not be in.
            hint("Off by default. When on, your Discord profile shows what you tick below and updates while you work, so people can also see when you're active. Anyone who can see your profile can keep what was shown; turning this off can't take it back. Hidden clients and days with no usage are never shown.")
            toggleRow("Include today's tokens", isOn: componentBinding(.tokens))
            toggleRow("Include the client name", isOn: componentBinding(.client))
            toggleRow("Include cost", isOn: componentBinding(.cost))
            // Not a hint about tidiness. Unticking everything is the one
            // combination that would otherwise still publish — an activity
            // carrying the app name, image and button, refreshing while you
            // work — so it is treated as switching the feature off for as long
            // as it stays empty.
            hint("Untick everything to show nothing.")
            // Read here, not inside the options expression: `selection()` goes
            // to UserDefaults and registers no SwiftUI dependency, so the list
            // would keep a de-listed selection visible after the user picked
            // another row. Touching the @AppStorage raw is what re-renders.
            let selectionRaw = discordSelectionRaw
            let discordSelection = DiscordPresence.selection()
            radioGroup(
                selection: Binding(
                    get: {
                        // Read for the SwiftUI dependency only; the ANSWER comes
                        // from the strict accessor. `@AppStorage<String>`
                        // substitutes its empty default for a key holding a
                        // non-string, which would tick "most used" while the
                        // runtime published nothing at all.
                        _ = selectionRaw
                        switch discordSelection {
                        case .mostUsed: return ""
                        case .only(let id): return id
                        case .malformed: return DiscordPresence.malformedSelectionLabel
                        }
                    },
                    set: { discordSelectionRaw = $0 }),
                options: [("", "Whichever client you used most")]
                    + DiscordPresence.selectableClients(
                        present: presentClients,
                        hiddenRaw: tabsHiddenRaw,
                        orderRaw: tabsOrderRaw,
                        selection: discordSelection
                    ).map { ($0, ClientRegistry.style($0).displayName) })
            // Nothing is ticked when the stored value is malformed, which is
            // honest: the runtime publishes nothing, and no option describes
            // that. Picking any row writes a well-formed value and recovers.
            // Two consequences, and neither is obvious from the control. The
            // first reads as a bug when it is a decision; the second is the one
            // that compounds with the switch below it.
            hint("Picking one client shows only its usage and spend, so the numbers may not match the menu bar.")
            toggleRow("Show cost as a figure instead of a range", isOn: $discordWholeDollars)
            // Says what the trade is, not that there is one. A range puts you
            // in a group; a figure is closer to a value only you have, and a
            // sequence of them across weeks is closer still.
            hint("A range is shared by many people. A whole-dollar amount says more about you.")
        }
        .id(SettingsWindowController.Destination.discordAnchor)

        claudeExtraRootsSection()

        antigravityAccountsSection()

        cursorSyncSection()

        section("Language") {
            radioGroup(
                selection: Binding(
                    get: { languageRaw },
                    set: { next in
                        guard AppLanguage.requiresRelaunch(
                            from: languageRaw, to: next)
                        else { return }
                        languageRaw = next
                        AppLanguage(rawValue: next)?.apply()
                        showLanguageRestartPrompt = true
                    }),
                options: AppLanguage.allCases.map { ($0.rawValue, $0.label) })
            hint("Takes effect the next time Syrtis starts.")
        }
    }

    /// A second (or further) Claude account isolated with `CLAUDE_CONFIG_DIR`
    /// is otherwise invisible to Syrtis's scan (see `docs/knowledge/
    /// architecture.md`'s extra-scan-paths section). Each config dir here
    /// expands to its `projects`/`transcripts` sub-roots and merges into the
    /// single reported total — there is no per-account breakdown.
    @ViewBuilder
    private func claudeExtraRootsSection() -> some View {
        section("Claude accounts") {
            ForEach(claudeExtraRoots, id: \.self) { path in
                row(path) {
                    HStack(spacing: 6) {
                        if missingClaudeRoots.contains(path) {
                            Image(systemName: "exclamationmark.triangle.fill")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                                .help("Folder not found. Kept in case the drive is disconnected or the path needs fixing.".localized)
                        }
                        Button {
                            claudeExtraRoots.removeAll { $0 == path }
                            commitClaudeExtraRoots()
                        } label: {
                            Image(systemName: "minus.circle")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
            Button {
                addClaudeExtraRoot()
            } label: {
                Label("Add config dir…".localized, systemImage: "plus.circle")
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .padding(.horizontal, 10)
            if let result = claudeExtraRootsResult, !result.unreadable.isEmpty {
                hint("%lld of %lld folders can't be read right now; Syrtis will retry on the next scan.".localized(
                    result.unreadable.count, result.registeredCount))
            }
            if let result = claudeExtraRootsResult, !result.rejected.isEmpty {
                hint("%lld path(s) can't be scanned and weren't added.".localized(
                    result.rejected.count))
            }
            hint("For another Claude account, run Claude Code with CLAUDE_CONFIG_DIR set to its own folder and add that folder here. Its usage joins your totals; accounts aren't shown separately.")
        }
        .onAppear { refreshMissingClaudeRoots() }
    }

    /// Cursor usage synced from the Cursor desktop app. The hint is the same
    /// privacy paragraph the one-time notice shows (`CursorSync.Copy.privacy`).
    @ViewBuilder
    private func cursorSyncSection() -> some View {
        section(CursorSync.Copy.title) {
            toggleRow(
                CursorSync.Copy.toggle,
                isOn: Binding(get: { cursorSyncOn }, set: { cursorSync.setEnabled($0) }))
            hint(CursorSync.Copy.privacy)
            if cursorSyncOn {
                if let line = CursorSync.statusLine(
                    state: cursorSync.state, lastSuccessMs: cursorSync.lastSuccessMs)
                {
                    Text(line)
                        .font(.caption2)
                        .foregroundStyle(cursorSync.state == "ok" ? Color.secondary : Color.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if cursorSync.state == "cliPresent", !cursorTakeover {
                    hint(CursorSync.Copy.cliQuestion)
                    Button(CursorSync.Copy.useSyrtis.localized) {
                        cursorSync.setTakeoverConfirmed(true)
                    }
                    .controlSize(.small)
                    .padding(.horizontal, 10)
                } else if cursorTakeover {
                    // Undo for the D6 answer; the question returns on the next sync.
                    Button(CursorSync.Copy.keepCLI.localized) {
                        cursorSync.setTakeoverConfirmed(false)
                    }
                    .controlSize(.small)
                    .padding(.horizontal, 10)
                }
                Button {
                    Task { await cursorSync.runSync(explicit: true) }
                } label: {
                    Text((cursorSync.syncing ? CursorSync.Copy.syncing : CursorSync.Copy.syncNow).localized)
                        .font(.caption)
                }
                .buttonStyle(.plain)
                .disabled(cursorSync.syncing || !cursorNoticeAck)
                .padding(.horizontal, 10)
            }
        }
    }

    /// Extra Google accounts for Antigravity, each its own card. The copy says
    /// what is read, where it is kept, where it is sent and what Remove does;
    /// each claim is checked against `agent_antigravity.rs`'s captured-account
    /// section, so change them together.
    @ViewBuilder
    private func antigravityAccountsSection() -> some View {
        let accounts = AntigravityAccounts.decode(antigravityAccountsRaw)
        let busy = antigravityAutoCapture.busy
        section("Antigravity accounts") {
            ForEach(accounts, id: \.key) { account in
                row(account.label) {
                    Button {
                        Task { await antigravityAutoCapture.remove(account) }
                    } label: {
                        Image(systemName: "minus.circle")
                            .foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                    .disabled(busy)
                    .help("Remove".localized)
                }
            }
            Button {
                Task { await antigravityAutoCapture.manualCapture() }
            } label: {
                Label(
                    (busy ? "Capturing…" : "Capture current agy login").localized,
                    systemImage: "plus.circle")
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .disabled(busy)
            .padding(.horizontal, 10)
            if let message = antigravityAutoCapture.message {
                Text(message.localized)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            toggleRow(
                "Capture accounts agy signs into automatically",
                isOn: Binding(
                    get: { antigravityAutoCaptureOn },
                    set: { on in Task { await antigravityAutoCapture.setEnabled(on) } }))
            if antigravityAutoCaptureOn, antigravityAutoCapture.paused {
                Text("Automatic capture is paused because the login keychain didn't answer. Press Capture to resume.".localized)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            } else if antigravityAutoCaptureOn, antigravityAutoCapture.unavailable {
                Text("Automatic capture is unavailable on this Mac.".localized)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if !antigravityAutoCaptureOn {
                // Without it a manual Capture's merge lasts only until agy
                // rewrites its login item when it refreshes its sign-in
                // (measured: about hourly), because the marker moves and
                // Syrtis may not re-read agy's login to confirm the account.
                hint("Turn this on to keep agy's current account merged into one card. With it off, a Capture merges it only until agy next refreshes its sign-in, usually within an hour.")
            }
            hint("When on, Syrtis copies the sign-in of each account agy signs into to this Mac's login keychain: once when you turn this on, then whenever agy's sign-in changes. Turning it off keeps the copies. An account you remove stays removed until you press Capture.")
            hint("To add another Google account:\n1. Sign agy in to that account.\n2. Press Capture current agy login.\n3. Sign agy back in to your main account.")
            hint("Capture reads agy's saved Google login once when you press the button, and once each time agy's sign-in changes while automatic capture is on. The copy is kept in this Mac's login keychain. Syrtis uses it only with Google's token service and Cloud Code quota service, and its requests identify as Antigravity.")
            hint("Remove deletes only the copy on this Mac. Google still accepts it until you revoke access in Google Account → third-party access, which also signs Antigravity out of that account.")
        }
    }

    private func addClaudeExtraRoot() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add".localized
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let path = url.path
        guard !ClaudeExtraRoots.isRejectedRoot(path), !claudeExtraRoots.contains(path) else { return }
        claudeExtraRoots.append(path)
        commitClaudeExtraRoots()
    }

    private func commitClaudeExtraRoots() {
        ClaudeExtraRoots.save(claudeExtraRoots)
        ClaudeExtraRoots.apply { claudeExtraRootsResult = $0 }
        refreshMissingClaudeRoots()
    }

    private func refreshMissingClaudeRoots() {
        ClaudeExtraRoots.missingRoots(in: claudeExtraRoots) { missingClaudeRoots = $0 }
    }

    @ViewBuilder
    private func aboutPage() -> some View {
        section("About") {
            row("Version") {
                Text(AppInfo.version)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if UpdaterService.isAvailable {
                row("Check for updates") {
                    Button("Check Now") { UpdaterService.shared.checkForUpdates() }
                        .controlSize(.small)
                }
                row("Receive beta updates") {
                    Toggle("", isOn: $betaUpdates)
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                }
            }
            hint("Syrtis started as a fork of tokcat by handlecusion. Log parsing and pricing come from tokscale by Junho Yeo, the menu bar design draws on CodexBar by Peter Steinberger, and the running cat comes from RunCat by Takuto Nakamura. MIT licensed.")
        }
    }

    private var isAnimatedStyle: Bool {
        TrayAnimator.animatedStyles.contains(animationStyle)
    }

    /// Shows the resolved auto height while 0 (auto), the chosen value once set.
    private var popoverHeightBinding: Binding<Double> {
        Binding(
            get: {
                popoverHeight > 0
                    ? popoverHeight
                    : Double(PopoverChrome.autoHeight(
                        visibleHeight: NSScreen.main?.visibleFrame.height ?? 900))
            },
            set: { popoverHeight = $0 })
    }

    /// Slider ceiling: the screen the settings window is on (the controller
    /// re-clamps to the popover's actual screen on open anyway).
    private var popoverHeightMax: Double {
        Double(max(700, (NSScreen.main?.visibleFrame.height ?? 1000) - 24))
    }

    @ViewBuilder
    private func quotaSourcePicker() -> some View {
        let canonical = QuotaResolver.canonicalSelection(
            payload: agentUsage, selection: quotaSource)
        let selectedClientId = quotaClientId(from: canonical)
        // Primary accounts only: this picker writes one global selection
        // string with no account component, and an extra account can offer
        // windows carded identically to the primary's (both a "session.v1"),
        // so this list must not offer a choice it cannot actually distinguish
        // once persisted. An extra account's windows remain visible in the
        // Agent-limits overview.
        let agents = (agentUsage?.agents ?? []).filter {
            $0.error == nil && $0.accountKey == nil && !$0.uniqueCardWindows.isEmpty
        }
        let availableClientIds = agents.map(\.clientId)
        let clientIds = selectedClientId.map {
            availableClientIds.contains($0) ? availableClientIds : availableClientIds + [$0]
        } ?? availableClientIds
        let selectedAgent = selectedClientId.flatMap { selectedId in
            agents.first { $0.clientId == selectedId }
        }

        row("Agent") {
            Picker("", selection: Binding(
                get: { selectedClientId ?? QuotaResolver.auto },
                set: { next in
                    if next == QuotaResolver.auto {
                        quotaSource = QuotaResolver.auto
                    } else if next != selectedClientId,
                              let agent = agents.first(where: { $0.clientId == next }),
                              let window = agent.uniqueCardWindows.first
                    {
                        quotaSource = QuotaResolver.selection(
                            clientId: agent.clientId, cardId: window.cardId)
                    }
                }))
            {
                Text("Auto (tightest window)".localized)
                    .tag(QuotaResolver.auto)
                ForEach(clientIds, id: \.self) { clientId in
                    Text(ClientRegistry.style(clientId).displayName)
                        .tag(clientId)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .frame(maxWidth: 190)
        }

        if let selectedClientId {
            row("Window") {
                if let selectedAgent {
                    let availableSelections = Set(selectedAgent.uniqueCardWindows.map {
                        QuotaResolver.selection(
                            clientId: selectedClientId, cardId: $0.cardId)
                    })
                    Picker("", selection: Binding(
                        get: { canonical },
                        set: { next in
                            quotaSource = QuotaResolver.canonicalSelection(
                                payload: agentUsage, selection: next)
                        }))
                    {
                        ForEach(selectedAgent.uniqueCardWindows, id: \.cardId) { window in
                            Text(window.label.localized)
                                .tag(QuotaResolver.selection(
                                    clientId: selectedClientId, cardId: window.cardId))
                        }
                        if !availableSelections.contains(canonical) {
                            Text("Unavailable selection".localized)
                                .tag(canonical)
                                .disabled(true)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .frame(maxWidth: 190)
                } else {
                    Text("—")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private func quotaClientId(from selection: String) -> String? {
        guard selection != QuotaResolver.auto else { return nil }
        return selection.split(
            separator: "|", maxSplits: 1, omittingEmptySubsequences: false
        ).first.map(String.init)
    }

    // MARK: - Building blocks

    private func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.localized.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
            content()
        }
    }

    private func row(_ label: String, @ViewBuilder trailing: () -> some View) -> some View {
        HStack {
            Text(label.localized)
                .font(.caption)
            Spacer()
            trailing()
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .glassCard(cornerRadius: 8)
    }

    /// One checkbox over the shared composition string. Written back in
    /// `Component.allCases` order so the stored value is canonical whatever
    /// order the boxes were ticked in, and the value gate does not see a
    /// reordering as a change.
    private func componentBinding(_ component: DiscordPresence.Component) -> Binding<Bool> {
        Binding(
            get: {
                // `discordComponentsRaw` is read for the SwiftUI dependency
                // only; the ANSWER comes from the authoritative accessor.
                // `@AppStorage<String>` substitutes its default for both an
                // absent key and one holding a non-string, while
                // `components()` distinguishes them — so reading the wrapper
                // for the answer would tick all three boxes over a malformed
                // write while the runtime published nothing, and the next tick
                // would start from that phantom all-selected state.
                _ = self.discordComponentsRaw
                return DiscordPresence.components().contains(component)
            },
            set: { isOn in
                var selected = DiscordPresence.components()
                if isOn { selected.insert(component) } else { selected.remove(component) }
                self.discordComponentsRaw = DiscordPresence.rawComponents(selected)
            })
    }

    private func toggleRow(_ label: String, isOn: Binding<Bool>) -> some View {
        row(label) {
            Toggle("", isOn: isOn)
                .toggleStyle(.switch)
                .controlSize(.mini)
                .labelsHidden()
        }
    }

    private func radioGroup(
        selection: Binding<String>, options: [(value: String, label: String)]
    ) -> some View {
        VStack(spacing: 1) {
            ForEach(options, id: \.value) { option in
                radioOption(
                    selection: selection,
                    value: option.value,
                    label: option.label)
            }
        }
        .glassCard(cornerRadius: 8)
    }

    private func radioOption(
        selection: Binding<String>, value: String, label: String
    ) -> some View {
        Button {
            selection.wrappedValue = value
        } label: {
            HStack {
                Text(label.localized)
                    .font(.caption)
                Spacer()
                if selection.wrappedValue == value {
                    Image(systemName: "checkmark")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(Color.accentColor)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func hint(_ text: String) -> some View {
        Text(text.localized)
            .font(.caption2)
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// The three chips select a level in one shared editor below the color row.
private struct MenuBarTextColorControl: View {
    let level: QuotaColorLevel
    @Binding var hex: String
    @Binding var editingLevel: QuotaColorLevel?

    var body: some View {
        VStack(spacing: 3) {
            Text(level.label.localized)
                .font(.caption2)
                .foregroundStyle(.secondary)
            Button { editingLevel = editingLevel == level ? nil : level } label: {
                Capsule()
                    .fill(Color(hex: MenuBarTextColor.normalizedHex(hex) ?? level.defaultHex))
                    .frame(width: 42, height: 18)
                    .padding(3)
                    .background(.quaternary, in: Capsule())
            }
            .buttonStyle(.plain)
            .help(level.hint.localized)
            .accessibilityLabel(level.label.localized)
            .accessibilityValue(hex)
            .accessibilityIdentifier("settings.menuBar.textColor." + level.rawValue)
        }
    }
}

/// A compact editor shown from the color row; incomplete input stays local.
private struct MenuBarTextColorPopover: View {
    @Binding var hex: String
    let level: QuotaColorLevel
    @State private var input: String
    @FocusState private var inputFocused: Bool

    init(hex: Binding<String>, level: QuotaColorLevel) {
        _hex = hex
        self.level = level
        _input = State(initialValue: hex.wrappedValue)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(level.label.localized)
                .font(.caption)
                .foregroundStyle(.secondary)
            LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 6), count: 8), spacing: 6) {
                ForEach(MenuBarTextColor.presets, id: \.hex) { preset in
                    presetButton(name: preset.name, colorHex: preset.hex)
                }
            }
            HStack(spacing: 8) {
                Text("HEX").font(.caption)
                TextField("#000000", text: $input)
                    .font(.system(.caption, design: .monospaced))
                    .textFieldStyle(.roundedBorder)
                    .focused($inputFocused)
                    .accessibilityLabel("Hex color".localized)
                    .accessibilityIdentifier("settings.menuBar.textColor.hex")
                    .onSubmit {
                        if let normalized = MenuBarTextColor.normalizedHex(input) {
                            input = normalized
                        }
                    }
            }
            if !input.isEmpty, MenuBarTextColor.normalizedHex(input) == nil {
                Text("Enter a 6-digit hex color, e.g. #000000.")
                    .font(.caption2)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(width: 224)
        .padding(12)
        .onAppear { input = hex }
        .onChange(of: input) { _, next in
            if let normalized = MenuBarTextColor.normalizedHex(next), normalized != hex {
                hex = normalized
            }
        }
        .onChange(of: hex) { _, next in input = next }
        .onChange(of: inputFocused) { _, focused in
            if !focused, MenuBarTextColor.normalizedHex(input) == nil {
                input = ""
            }
        }
    }

    private func presetButton(name: String, colorHex: String) -> some View {
        let selected = MenuBarTextColor.normalizedHex(hex) == colorHex
        let color = Color(hex: colorHex)
        return Button {
            hex = colorHex
            input = colorHex
        } label: {
            Circle()
                .fill(color)
                .overlay(Circle().strokeBorder(Color.primary.opacity(0.2), lineWidth: 1))
                .frame(width: 18, height: 18)
                .padding(2)
                .background(Circle().stroke(selected ? Color.accentColor : Color.clear, lineWidth: 2))
        }
        .buttonStyle(.plain)
        .help(name.localized + " " + colorHex)
        .accessibilityLabel(name.localized + " " + colorHex)
        .accessibilityIdentifier("settings.menuBar.textColor." + String(colorHex.dropFirst()))
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// Build/version info. The bare SwiftPM executable has no bundle, so the
/// version is a constant until Phase 9 wraps it in a .app with an Info.plist.
enum AppInfo {
    static var version: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "dev"
    }

    /// Read from the bundle rather than hard-coded, so a rename carries into the
    /// UI with the Info.plist instead of leaving a stale name behind.
    static var name: String {
        Bundle.main.infoDictionary?["CFBundleName"] as? String ?? "Syrtis"
    }
}
