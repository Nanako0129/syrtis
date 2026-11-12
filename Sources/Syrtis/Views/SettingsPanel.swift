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
        case accounts = "Accounts"
        case devices = "Devices"
        case general = "General"
        case about = "About"

        var id: Self { self }

        var localizedTitle: String { rawValue.localized }

        var symbolName: String {
            switch self {
            case .menuBar: "menubar.rectangle"
            case .dashboard: "chart.bar.xaxis"
            case .usageAttribution: "arrow.triangle.branch"
            case .accounts: "person.2"
            case .devices: "laptopcomputer.and.iphone"
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
    @AppStorage(TrayAnimator.windowLetterKey) private var showsWindowLetter = true
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto
    @AppStorage(ClientTray.hideMainItemKey) private var hideMainItem = false
    @AppStorage(ClientTray.displayStyleKey) private var individualDisplayStyle = ClientTray.DisplayStyle.percentage.rawValue
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
    /// `ClaudeExtraRoots.save` and the shared `ExtraScanRoots.apply`
    /// immediately (D1: no restart).
    @State private var claudeExtraRoots: [String] = ClaudeExtraRoots.load()
    @State private var claudeExtraRootsResult: ExtraScanPathsResult?
    /// A refused add: its fixed sentence (`AccountsCopy.refusalSentence`),
    /// shown in an alert (`.alert` on the Claude section serves every list).
    @StateObject private var addRefusal = AddRefusalAlert()
    /// An Add is asking the core: one at a time.
    @State private var claudeAdding = false
    /// The core's answer for the stored Claude list, recorded by every apply
    /// (its rule reads the disk, so it is asked off the main thread); a page
    /// that appears before any apply has answered asks once itself.
    @ObservedObject private var claudeRoots = ClaudeRootsListModel.shared
    /// Filled off the main actor — see `ClaudeExtraRoots.missingRoots`. Empty
    /// until the first probe lands, so a row shows no warning rather than a
    /// wrong one while the answer is still unknown.
    @State private var missingClaudeRoots: Set<String> = []
    /// Additional Pi roots, same lifecycle as the Claude list above. The lists
    /// are applied together because `tb_set_extra_scan_paths` is full-replace.
    @State private var piExtraRoots: [String] = ExtraScanRoots.Pi.load()
    @State private var piExtraRootsResult: ExtraScanPathsResult?
    @State private var missingPiRoots: Set<String> = []
    /// Additional DeepSeek Harness session folders, same lifecycle again.
    @State private var dshExtraRoots: [String] = ExtraScanRoots.Dsh.load()
    @State private var dshExtraRootsResult: ExtraScanPathsResult?
    @State private var missingDshRoots: Set<String> = []
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
            case .accounts:
                accountsPage()
            case .devices:
                DevicesSettingsView(panel: self)
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
                toggleRow("Show window length letter", isOn: $showsWindowLetter)
                hint("A letter before the gauge names the window it follows: H for 5 hours, D for a day, W for a week, M for a month. No letter while the window's length is unknown.")
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
            hidden: ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(tabsHiddenRaw)),
            orderRaw: tabsOrderRaw,
            officialClients: AgentIconView.availableOfficialClientIDs())

        section("Individual items") {
            toggleRow("Hide main menu bar item", isOn: $hideMainItem)
            hint("The main item stays visible when all individual items are disabled or hidden.")
            row("Individual item display") {
                Picker("Individual item display", selection: $individualDisplayStyle) {
                    ForEach(ClientTray.DisplayStyle.allCases, id: \.rawValue) { style in
                        Text(style.label.localized).tag(style.rawValue)
                    }
                }
                .labelsHidden()
                .pickerStyle(.menu)
                .frame(maxWidth: 210)
                .accessibilityLabel("Individual item display".localized)
            }
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
                    let tabHiddenSet = ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(tabsHiddenRaw))
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
            let hiddenSet = ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(tabsHiddenRaw))

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
                            // Rows that are not tab ids — a grouped member
                            // that owns its own quota card, for example — have
                            // no tab of their own, so they show a caption
                            // instead of the switch.
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
                                            tabsHiddenRaw = ClientRegistry.updatingTabVisibility(
                                                ClientRegistry.parseIdSet(tabsHiddenRaw),
                                                tab: id, visible: show
                                            ).sorted().joined(separator: ",")
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
            hint("Drag to order tabs and quota cards. The switch hides a client's tab and its quota card. Rows marked (quota card only) have no tab of their own; moving one also moves the tab it belongs to, because the first row of a group sets that group's tab position.")
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

        if GlassTintSlider.glassAvailable {
            section("Glass tint") {
                GlassTintSlider()
                .padding(.horizontal, 10)
                .padding(.vertical, 8)
                .glassCard(cornerRadius: 8)
                hint("Makes the popover's glass less see-through, so its text stays readable over the windows behind it.")
            }
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

    /// Multi-account management. Each section keeps one line of description;
    /// the rest lives behind its ⓘ (see `infoSection`).
    @ViewBuilder
    private func accountsPage() -> some View {
        claudeExtraRootsSection()
            // One alert for every list's refused add: the approved sentence is
            // the title (the system adds the localized OK button).
            .alert(
                addRefusal.title.localized,
                isPresented: Binding(
                    get: { addRefusal.message != nil }, set: { if !$0 { addRefusal.dismiss() } }),
                actions: {})

        antigravityAccountsSection()

        piExtraRootsSection()

        dshExtraRootsSection()

        cursorSyncSection()
    }

    /// A second (or further) Claude account isolated with `CLAUDE_CONFIG_DIR`
    /// is otherwise invisible to Syrtis's scan (see `docs/knowledge/
    /// architecture.md`'s extra-scan-paths section). Each config dir here
    /// expands to its `projects`/`transcripts` sub-roots, whose usage merges
    /// into the single reported total; the folder's quota shows as its own
    /// Claude card and account pill (`tb_set_claude_config_dirs`).
    @ViewBuilder
    private func claudeExtraRootsSection() -> some View {
        infoSection(
            AccountsCopy.claude, summary: AccountsCopy.claudeSummary,
            details: { ForEach(AccountsCopy.claudeDetails, id: \.self) { hint($0) } }
        ) {
            // The core's refusals for the stored list, from `claudeRoots` (one
            // ask per list, off the main thread; no marks until it answers). A
            // refused entry stays listed, marked, so nothing the user added
            // disappears silently; it reaches neither registry.
            let refusedClaudeRoots = claudeRoots.refused(for: claudeExtraRoots)
            ForEach(Array(claudeExtraRoots.enumerated()), id: \.element) { index, path in
                row(path) {
                    HStack(spacing: 6) {
                        if refusedClaudeRoots[index] != nil {
                            Image(systemName: "xmark.octagon.fill")
                                .font(.caption2)
                                .foregroundStyle(.red)
                                .help(AccountsCopy.refusalSentence(refusedClaudeRoots[index] ?? "").localized)
                                .accessibilityLabel(AccountsCopy.refusalSentence(refusedClaudeRoots[index] ?? "").localized)
                        } else if missingClaudeRoots.contains(path) {
                            Image(systemName: "exclamationmark.triangle.fill")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                                .help(AccountsCopy.folderMissing.localized)
                        }
                        Button {
                            claudeExtraRoots.removeAll { $0 == path }
                            commitClaudeExtraRoots()
                        } label: {
                            Image(systemName: "minus.circle")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .help(AccountsCopy.removeHelp.localized)
                        .accessibilityLabel(AccountsCopy.removeName(path))
                    }
                }
            }
            Button {
                addClaudeExtraRoot()
            } label: {
                Label(AccountsCopy.addConfigDir.localized, systemImage: "plus.circle")
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .disabled(claudeAdding)
            .padding(.horizontal, 10)
            // Per-client share of the merged result — the raw counts span both
            // clients' roots. See `ExtraScanRoots.clientHint`.
            // No hint until the core has answered for this list: a guessed
            // count would show a wrong "of N registered".
            let claudeHint = claudeRoots.expandedCount(for: claudeExtraRoots).flatMap {
                ExtraScanRoots.clientHint(claudeExtraRootsResult, client: "claude", requested: $0)
            }
            if let claudeHint, claudeHint.unreadable > 0 {
                hint(AccountsCopy.unreadableFolders.localized(
                    claudeHint.unreadable, claudeHint.registered))
            }
            if let claudeHint, claudeHint.rejected > 0 {
                hint(AccountsCopy.rejectedPaths.localized(
                    claudeHint.rejected))
            }
        }
        .onAppear { refreshMissingClaudeRoots() }
        .task(id: claudeExtraRoots) { await claudeRoots.refresh(claudeExtraRoots) }
    }

    /// Extra directories of Pi session-format JSONL files. A headless runner
    /// can write a usage-only export here instead of a normal session file
    /// under `~/.pi/agent/sessions`; the existing Pi parser reads it without
    /// distinction. No default root and no discovery: the list starts empty.
    @ViewBuilder
    private func piExtraRootsSection() -> some View {
        infoSection(
            AccountsCopy.pi, summary: AccountsCopy.piSummary,
            details: { ForEach(AccountsCopy.piDetails, id: \.self) { hint($0) } }
        ) {
            ForEach(piExtraRoots, id: \.self) { path in
                row(path) {
                    HStack(spacing: 6) {
                        if missingPiRoots.contains(path) {
                            Image(systemName: "exclamationmark.triangle.fill")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                                .help(AccountsCopy.folderMissing.localized)
                        }
                        Button {
                            piExtraRoots.removeAll { $0 == path }
                            commitPiExtraRoots()
                        } label: {
                            Image(systemName: "minus.circle")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .help(AccountsCopy.removeHelp.localized)
                        .accessibilityLabel(AccountsCopy.removeName(path))
                    }
                }
            }
            Button {
                addPiExtraRoot()
            } label: {
                Label(AccountsCopy.addFolder.localized, systemImage: "plus.circle")
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .padding(.horizontal, 10)
            // Per-client share of the merged result — the raw counts span both
            // clients' roots. See `ExtraScanRoots.clientHint`.
            let piHint = ExtraScanRoots.clientHint(
                piExtraRootsResult, client: "pi", requested: piExtraRoots.count)
            if let piHint, piHint.unreadable > 0 {
                hint(AccountsCopy.unreadableFolders.localized(
                    piHint.unreadable, piHint.registered))
            }
            if let piHint, piHint.rejected > 0 {
                hint(AccountsCopy.rejectedPaths.localized(
                    piHint.rejected))
            }
        }
        .onAppear { refreshMissingPiRoots() }
    }

    /// Extra DeepSeek Harness homes. Each root is a `sessions` folder; its
    /// usage merges into the single DSH total (the engine dedups a session
    /// reachable under two roots).
    @ViewBuilder
    private func dshExtraRootsSection() -> some View {
        infoSection(
            AccountsCopy.dsh, summary: AccountsCopy.dshSummary,
            details: { hint(AccountsCopy.dshDetails) }
        ) {
            ForEach(dshExtraRoots, id: \.self) { path in
                row(path) {
                    HStack(spacing: 6) {
                        if missingDshRoots.contains(path) {
                            Image(systemName: "exclamationmark.triangle.fill")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                                .help(AccountsCopy.folderMissing.localized)
                        }
                        Button {
                            dshExtraRoots.removeAll { $0 == path }
                            commitDshExtraRoots()
                        } label: {
                            Image(systemName: "minus.circle")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .help(AccountsCopy.removeHelp.localized)
                        .accessibilityLabel(AccountsCopy.removeName(path))
                    }
                }
            }
            Button {
                addDshExtraRoot()
            } label: {
                Label(AccountsCopy.addFolder.localized, systemImage: "plus.circle")
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .padding(.horizontal, 10)
            let dshHint = ExtraScanRoots.clientHint(
                dshExtraRootsResult, client: "dsh", requested: dshExtraRoots.count)
            if let dshHint, dshHint.unreadable > 0 {
                hint(AccountsCopy.unreadableFolders.localized(
                    dshHint.unreadable, dshHint.registered))
            }
            if let dshHint, dshHint.rejected > 0 {
                hint(AccountsCopy.rejectedPaths.localized(
                    dshHint.rejected))
            }
        }
        .onAppear { refreshMissingDshRoots() }
    }

    /// Cursor usage synced from the Cursor desktop app. The privacy paragraph
    /// is the one the one-time notice shows (`CursorSync.Copy.privacy`).
    @ViewBuilder
    private func cursorSyncSection() -> some View {
        infoSection(
            AccountsCopy.cursor, summary: AccountsCopy.cursorSummary,
            details: {
                hint(CursorSync.Copy.privacy)
                hint(CursorSync.Copy.profilesPrivacy)
            }
        ) {
            // Consent: turning the switch on opens a confirmation carrying the
            // privacy paragraph; nothing is enabled until Continue. Cancel,
            // Escape, clicking away or leaving the page leaves the switch off.
            // All state lives in the controller (`confirmPending`). Checked by
            // hand in a preview build of 394ada5a (2026-11-01): Return does not
            // consent, and leaving the page does not bring the prompt back.
            toggleRow(
                CursorSync.Copy.toggle,
                isOn: Binding(
                    get: { CursorSync.toggleShowsOn(enabled: cursorSyncOn, acknowledged: cursorNoticeAck) },
                    set: { cursorSync.switchChanged(to: $0) }))
                .popover(
                    isPresented: Binding(
                        get: { cursorSync.confirmPending },
                        set: { if !$0 { cursorSync.cancelEnable() } }),
                    arrowEdge: .bottom
                ) {
                    VStack(alignment: .leading, spacing: 10) {
                        // Added profile folders are read too, so their
                        // paragraph joins the consent once any is stored.
                        ForEach(CursorSync.consentParagraphs(profiles: cursorSync.profiles), id: \.self) { hint($0) }
                        HStack {
                            Spacer()
                            Button(CursorSync.Copy.cancel.localized) { cursorSync.cancelEnable() }
                                .keyboardShortcut(.cancelAction)
                            // No default-action shortcut: Return must not consent.
                            Button(CursorSync.Copy.continue.localized) { cursorSync.confirmEnable() }
                        }
                        .controlSize(.small)
                    }
                    .frame(width: 280, alignment: .leading)
                    .padding(12)
                }
                .onDisappear { cursorSync.cancelEnable() }
            if CursorSync.toggleShowsOn(enabled: cursorSyncOn, acknowledged: cursorNoticeAck) {
                if let line = CursorSync.statusLine(
                    state: cursorSync.state, lastSuccessMs: cursorSync.lastSuccessMs,
                    reason: cursorSync.reason)
                {
                    Text(line)
                        .font(.caption2)
                        .foregroundStyle(cursorSync.state == "ok" ? Color.secondary : Color.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
                // Cursor 3.27+ encrypted sign-in (plan CURSOR-SS-M, Design 8):
                // shown when the headline or any profile needs Keychain
                // consent. Allow opens nothing by itself: the confirmation
                // carries the disclosure, and only its Allow stores the answer
                // and starts the explicit sync that raises the macOS dialog.
                if CursorSync.needsKeychainAllow(reason: cursorSync.reason, profiles: cursorSync.profileStates) {
                    Button(CursorSync.Copy.keychainAllowAccess.localized) { cursorSync.askKeychainAccess() }
                        .controlSize(.small)
                        .padding(.horizontal, 10)
                        .popover(
                            isPresented: Binding(
                                get: { cursorSync.keychainConfirmPending },
                                set: { if !$0 { cursorSync.cancelKeychainAccess() } }),
                            arrowEdge: .bottom
                        ) {
                            VStack(alignment: .leading, spacing: 10) {
                                hint(CursorSync.Copy.keychainDisclosure)
                                HStack {
                                    Spacer()
                                    Button(CursorSync.Copy.cancel.localized) { cursorSync.cancelKeychainAccess() }
                                        .keyboardShortcut(.cancelAction)
                                    // No default-action shortcut: Return must not consent.
                                    Button(CursorSync.Copy.allow.localized) {
                                        Task { await cursorSync.allowKeychainAccess() }
                                    }
                                }
                                .controlSize(.small)
                            }
                            .frame(width: 280, alignment: .leading)
                            .padding(12)
                        }
                        .onDisappear { cursorSync.cancelKeychainAccess() }
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
                .disabled(cursorSync.syncing)
                .padding(.horizontal, 10)
            }
            if let line = CursorSync.cleanupLine(
                syncOn: CursorSync.toggleShowsOn(enabled: cursorSyncOn, acknowledged: cursorNoticeAck),
                cleanupFailed: cursorSync.cleanupFailed,
                staleCleanupFailed: cursorSync.staleCleanupFailed)
            {
                Text(line)
                    .font(.caption2)
                    .foregroundStyle(Color.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            cursorProfileRows()
        }
    }

    /// The default profile plus each added `--user-data-dir` folder. A row
    /// shows its own last outcome while sync is on, so an added profile's
    /// error is visible even when the headline above says something else.
    /// Editable while off; adding or removing then reaches no core call.
    @ViewBuilder
    private func cursorProfileRows() -> some View {
        let syncOn = CursorSync.toggleShowsOn(enabled: cursorSyncOn, acknowledged: cursorNoticeAck)
        cursorProfileRow(CursorSync.Copy.defaultProfile, id: "default", syncOn: syncOn) { EmptyView() }
        ForEach(cursorSync.profiles, id: \.id) { profile in
            cursorProfileRow(CursorSync.rowLabel(profile), id: profile.id, syncOn: syncOn) {
                Button {
                    cursorSync.removeProfile(id: profile.id)
                } label: {
                    Image(systemName: "minus.circle")
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .help(AccountsCopy.removeHelp.localized)
                .accessibilityLabel(AccountsCopy.removeName(CursorSync.rowLabel(profile)))
            }
        }
        Button {
            addCursorProfile()
        } label: {
            Label(CursorSync.Copy.addProfile.localized, systemImage: "plus.circle")
                .font(.caption)
        }
        .buttonStyle(.plain)
        .padding(.horizontal, 10)
    }

    @ViewBuilder
    private func cursorProfileRow(
        _ label: String, id: String, syncOn: Bool, @ViewBuilder trailing: () -> some View
    ) -> some View {
        row(label, trailing: trailing)
        if syncOn,
           let line = CursorSync.profileRowLine(
               id: id, refused: cursorSync.refusedProfiles, status: cursorSync.profileStates[id])
        {
            let calm = cursorSync.refusedProfiles[id] == nil
                && ["ok", "duplicate"].contains(cursorSync.profileStates[id]?.state ?? "")
            Text(line)
                .font(.caption2)
                .foregroundStyle(calm ? Color.secondary : Color.orange)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, 10)
        }
    }

    private func addCursorProfile() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = AccountsCopy.addPrompt.localized
        guard panel.runModal() == .OK, let url = panel.url else { return }
        if !cursorSync.addProfile(root: url.path) { addRefusal.show(CursorSync.Copy.cantAdd) }
    }

    /// Extra Google accounts for Antigravity, each its own card. The copy says
    /// what is read, where it is kept, where it is sent and what Remove does;
    /// each claim is checked against `agent_antigravity.rs`'s captured-account
    /// section, so change them together.
    @ViewBuilder
    private func antigravityAccountsSection() -> some View {
        let accounts = AntigravityAccounts.decode(antigravityAccountsRaw)
        let busy = antigravityAutoCapture.busy
        infoSection(
            AccountsCopy.antigravity, summary: AccountsCopy.antigravitySummary,
            details: {
                ForEach(AccountsCopy.antigravityGeneral, id: \.self) { hint($0) }
                // Last, so the paragraphs under the caption read as that
                // switch's. `autoOff` is shown only while the switch is off:
                // without it a manual Capture's merge lasts only until agy
                // rewrites its login item when it refreshes its sign-in
                // (measured: about hourly), because the marker moves and
                // Syrtis may not re-read agy's login to confirm the account.
                Text(AccountsCopy.autoToggle.localized)
                    .font(.caption2.weight(.semibold))
                ForEach(AccountsCopy.autoToggleGroup(autoCaptureOn: antigravityAutoCaptureOn), id: \.self) { hint($0) }
            }
        ) {
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
                    .help(AccountsCopy.removeHelp.localized)
                    .accessibilityLabel(AccountsCopy.removeName(account.label))
                }
            }
            Button {
                Task { await antigravityAutoCapture.manualCapture() }
            } label: {
                Label(
                    (busy ? AccountsCopy.capturing : AccountsCopy.captureCurrent).localized,
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
            // On asks first (`requestTurnOn`); nothing is written until the
            // user confirms, and the switch stays off meanwhile.
            toggleRow(
                AccountsCopy.autoToggle,
                isOn: Binding(
                    get: { antigravityAutoCaptureOn },
                    set: { on in
                        Task {
                            if on {
                                await antigravityAutoCapture.requestTurnOn()
                            } else {
                                await antigravityAutoCapture.setEnabled(false)
                            }
                        }
                    }))
                .popover(
                    isPresented: Binding(
                        get: { antigravityAutoCapture.confirmPending },
                        set: { if !$0 { antigravityAutoCapture.answerTurnOn(false) } }),
                    arrowEdge: .bottom
                ) {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(AccountsCopy.confirmTitle.localized)
                            .font(.callout.weight(.semibold))
                        hint(AccountsCopy.confirmBody)
                        HStack {
                            Spacer()
                            Button(AccountsCopy.confirmCancel.localized) {
                                antigravityAutoCapture.answerTurnOn(false)
                            }
                            .keyboardShortcut(.cancelAction)
                            // No default-action shortcut: Return must not consent.
                            Button(AccountsCopy.confirmOk.localized) {
                                antigravityAutoCapture.answerTurnOn(true)
                            }
                        }
                        .controlSize(.small)
                    }
                    .frame(width: 280, alignment: .leading)
                    .padding(12)
                }
                .onDisappear { antigravityAutoCapture.answerTurnOn(false) }
            if antigravityAutoCaptureOn, antigravityAutoCapture.paused {
                Text(AccountsCopy.autoPaused.localized)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            } else if antigravityAutoCaptureOn, antigravityAutoCapture.unavailable {
                Text(AccountsCopy.autoUnavailable.localized)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func addClaudeExtraRoot() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = AccountsCopy.addPrompt.localized
        guard panel.runModal() == .OK, let url = panel.url else { return }
        // The core's rule, asked (off the main thread) before anything is
        // saved; a refusal shows its sentence in an alert and saves nothing.
        let path = url.path
        claudeAdding = true
        Task {
            let line = await claudeRoots.add(
                path, current: { claudeExtraRoots }, set: { claudeExtraRoots = $0 })
            claudeAdding = false
            addRefusal.show(line)
            if line == nil { commitClaudeExtraRoots() }
        }
    }

    private func commitClaudeExtraRoots() {
        ClaudeExtraRoots.save(claudeExtraRoots)
        ExtraScanRoots.apply { claudeExtraRootsResult = $0 }
        refreshMissingClaudeRoots()
    }

    private func refreshMissingClaudeRoots() {
        ClaudeExtraRoots.missingRoots(in: claudeExtraRoots) { missingClaudeRoots = $0 }
    }

    private func addPiExtraRoot() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = AccountsCopy.addPrompt.localized
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let outcome = ExtraScanRoots.add(
            (url.path as NSString).standardizingPath, to: piExtraRoots,
            refusal: ExtraScanRoots.Pi.refusal(adding:to:))
        addRefusal.show(outcome.refusalLine)
        guard outcome.refusalLine == nil else { return }
        piExtraRoots = outcome.list
        commitPiExtraRoots()
    }

    private func commitPiExtraRoots() {
        ExtraScanRoots.Pi.save(piExtraRoots)
        ExtraScanRoots.apply { piExtraRootsResult = $0 }
        refreshMissingPiRoots()
    }

    private func refreshMissingPiRoots() {
        ExtraScanRoots.Pi.missingRoots(in: piExtraRoots) { missingPiRoots = $0 }
    }

    private func addDshExtraRoot() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = AccountsCopy.addPrompt.localized
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let outcome = ExtraScanRoots.add(
            (url.path as NSString).standardizingPath, to: dshExtraRoots,
            refusal: ExtraScanRoots.Dsh.refusal(adding:to:))
        addRefusal.show(outcome.refusalLine)
        guard outcome.refusalLine == nil else { return }
        dshExtraRoots = outcome.list
        commitDshExtraRoots()
    }

    private func commitDshExtraRoots() {
        ExtraScanRoots.Dsh.save(dshExtraRoots)
        ExtraScanRoots.apply { dshExtraRootsResult = $0 }
        refreshMissingDshRoots()
    }

    private func refreshMissingDshRoots() {
        ExtraScanRoots.Dsh.missingRoots(in: dshExtraRoots) { missingDshRoots = $0 }
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

    func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.localized.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
            content()
        }
    }

    /// `section` with a one-line description under the header and the long
    /// explanation behind an ⓘ popover. State notices stay in `content`.
    func infoSection(
        _ label: String, summary: String,
        @ViewBuilder details: () -> some View,
        @ViewBuilder content: () -> some View
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 4) {
                Text(label.localized.uppercased())
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.tertiary)
                SettingsInfoButton(section: label.localized, details: details())
            }
            hint(summary)
            content()
        }
    }

    func row(_ label: String, @ViewBuilder trailing: () -> some View) -> some View {
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

    func toggleRow(_ label: String, isOn: Binding<Bool>) -> some View {
        row(label) {
            Toggle("", isOn: isOn)
                .toggleStyle(.switch)
                .controlSize(.mini)
                .labelsHidden()
        }
    }

    func radioGroup(
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

    func hint(_ text: String) -> some View {
        Text(text.localized)
            .font(.caption2)
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// Accounts-page copy, shared by the view and SelfTest so the test reads what
/// the view shows. English is the catalog key. The Antigravity paragraphs
/// state privacy facts (keychain copy, Google services, revoke), checked
/// against `agent_antigravity.rs`'s captured-account section.
enum AccountsCopy {
    static let page = "Accounts"
    static let moreDetails = "More details"
    static let claude = "Claude"
    static let claudeSummary = "Extra accounts join your totals."
    // Copy below marked PENDING APPROVAL (Claude details, the refusal
    // sentences, Remove names): drafted in .agent-local/plans/
    // multi-account-mac-copy-draft.md with the code fact each sentence states;
    // the maintainer approves it verbatim before merge.
    /// The Claude ⓘ: how to add a folder, then the four questions a user asks
    /// of a credential read (where from, logged, sent where, written back).
    static let claudeDetails = [
        "For another Claude account, run Claude Code with CLAUDE_CONFIG_DIR set to its own folder and add that folder here. Each folder you add gets its own Claude card, and its usage also joins your totals.",
        "To show a folder's limits, Syrtis reads the Keychain item Claude Code keeps for it, named Claude Code-credentials- followed by 8 characters derived from the folder's path. macOS may ask you to allow this. Syrtis doesn't log the sign-in.",
        "Syrtis sends the sign-in's access token to api.anthropic.com to ask for the usage. Once that token has expired, Syrtis sends the refresh token to platform.claude.com for a new one and writes the new sign-in back into the same Keychain item.",
    ]

    /// One fixed sentence per refusal reason code: the core's
    /// `claude_config_dirs::Refusal` codes, Swift's fail-closed `unavailable`,
    /// and `home_or_ancestor` for Pi and DSH (home or a folder containing it).
    /// Shown in an alert when an add is refused and as the help of
    /// the mark on a stored Claude folder the core refuses. None carries a
    /// path or an OS error.
    static let refusalCodes = [
        "empty_path", "not_absolute", "filesystem_root", "home_unknown", "home_directory",
        "home_or_ancestor", "primary_scan_root", "duplicate", "overlap", "limit_reached", "unavailable",
    ]
    static func refusalSentence(_ code: String) -> String {
        switch code {
        case "empty_path": return "This folder can't be used: no folder was chosen."
        case "not_absolute": return "This folder can't be used: its path isn't a full path."
        case "filesystem_root": return "This folder can't be used: it is the top-level folder (/)."
        case "home_unknown": return "This folder can't be used: Syrtis couldn't find your home folder to check it."
        case "home_directory": return "This folder can't be used: it is your home folder."
        case "home_or_ancestor": return "This folder can't be used: it is your home folder or contains it."
        case "primary_scan_root":
            return "This folder can't be used: it is, contains or sits inside folders Syrtis already reads for your main Claude account."
        case "duplicate": return "This folder can't be used: it is already in the list."
        case "overlap": return "This folder can't be used: it contains, or sits inside, another folder in the list."
        case "limit_reached": return "This folder can't be used: the list already has 8 folders."
        default: return "This folder can't be used: Syrtis couldn't check it."
        }
    }
    static var refusalSentences: [String] { refusalCodes.map(refusalSentence) }

    /// Accessibility name of a row's Remove button: the row's full label, so
    /// two rows never share a name.
    static let removeNamed = "Remove %@"
    static func removeName(_ rowLabel: String) -> String { removeNamed.localized(rowLabel) }

    // Strings the Accounts page draws itself, kept here so `translated`
    // (and the SelfTest check over it) covers every one.
    static let folderMissing = "Folder not found. Kept in case the drive is disconnected or the path needs fixing."
    static let addConfigDir = "Add config dir…"
    static let addFolder = "Add folder…"
    static let addPrompt = "Add"
    static let removeHelp = "Remove"
    static let unreadableFolders = "%lld of %lld folders can't be read right now; Syrtis will retry on the next scan."
    static let rejectedPaths = "%lld path(s) can't be scanned and weren't added."
    static let capturing = "Capturing…"
    static let captureCurrent = "Capture current agy login"
    static let autoPaused = "Automatic capture is paused because the login keychain didn't answer. Press Capture to resume."
    static let autoUnavailable = "Automatic capture is unavailable on this Mac."
    /// `AntigravityAccounts.message(for:)`'s sentences, shown under the Capture button.
    static let agyGenericError = "Something went wrong. Try again."
    static let agyNotSignedIn = "Couldn't read agy's login. Check that agy is signed in, and allow access if macOS asks."
    static let agyLoginUnreadable = "agy's saved login is in a format Syrtis doesn't recognize."
    static let agyMissingIdentity = "agy's saved login doesn't say which Google account it is. Sign agy in again, then try again."
    static let agyClientNotFound = "Couldn't match this login to the installed Antigravity or agy."
    static let agyRejected = "Google didn't accept this login. Sign agy in again, then try again."
    static let agyUnreachable = "Google couldn't be reached right now. Try again later."
    static let agyMismatch = "Google answered for a different account. Nothing was saved."
    static let agyBadFormat = "The login had an unexpected format. Nothing was saved."
    static let agyWriteFailed = "Couldn't save to the login keychain."
    static let agyDeleteFailed = "Couldn't delete the copy from the login keychain."
    static let antigravityMessages = [
        agyGenericError, agyNotSignedIn, agyLoginUnreadable, agyMissingIdentity, agyClientNotFound,
        agyRejected, agyUnreachable, agyMismatch, agyBadFormat, agyWriteFailed, agyDeleteFailed,
    ]
    static let pi = "Pi"
    static let piSummary = "Add another Pi sessions folder; its usage joins your totals."
    static let piDetails = [
        "Directories of Pi session-format JSONL files. Usage-only exports from headless runners are counted, and full Pi sessions also work.",
        "Only assistant usage contributes. Roots merge into the Pi total; there is no per-root breakdown.",
    ]
    /// The registry's display name, so the header follows the product name.
    static var dsh: String { ClientRegistry.style("dsh").displayName }
    static let dshSummary = "Add another DSH home's sessions folder; its usage joins your totals."
    static let dshDetails = "Add the sessions folder of another DeepSeek Harness home, such as <DSH_HOME>/sessions. Its usage joins your DSH totals; homes aren't shown separately, and a session found in two folders counts once."
    static let cursor = "Cursor"
    static let cursorSummary = "Sync usage from the Cursor app."
    static let antigravity = "Antigravity"
    static let antigravitySummary = "Capture a Google account agy signs into."
    static let autoToggle = "Capture accounts agy signs into automatically"
    static let recipe = "To add another Google account:\n1. Sign agy in to that account.\n2. Press Capture current agy login.\n3. Sign agy back in to your main account."
    // Copy below marked PENDING APPROVAL: drafted in .agent-local/plans/
    // multi-account-mac-copy-draft.md with the code fact each sentence
    // states; the maintainer approves it verbatim before merge.
    static let captureReads = "Capture reads agy's saved Google sign-in once when you press the button and, while automatic capture is on, again each time agy rewrites its saved sign-in (including its routine refresh). macOS may ask you to allow this. The copy is kept in this Mac's login keychain. Syrtis sends its refresh token to Google's token service and the access token it gets back to Google's Cloud Code quota service, where its requests identify as Antigravity."
    static let autoOn = "When on, Syrtis reads agy's sign-in once when you turn this on, then again each time agy rewrites its saved sign-in (including its routine refresh), and keeps a copy of each account in this Mac's login keychain, updated when it changes. Turning it off keeps the copies. An account you remove while this is on stays removed until you sign agy in to it and press Capture."
    /// The confirmation before the switch turns on (security review S12/S13).
    static let confirmTitle = "Turn on automatic capture?"
    static let confirmBody = "Syrtis reads agy's current sign-in now and again each time agy rewrites its saved sign-in, and keeps a copy of each account in this Mac's login keychain without asking again. To check a new sign-in, Syrtis sends agy's refresh token to Google's token service. macOS may ask you to allow access to agy's keychain item. Accounts you remove while this is on stay removed until you sign agy in to them and press Capture."
    static let confirmOk = "Turn On"
    static let confirmCancel = "Cancel"
    static let autoOff = "Turn this on to keep reading agy's current account through its captured sign-in, shown as one OAUTH card, so Syrtis rarely needs to run agy. With it off, a Capture lasts only until agy next refreshes its sign-in (usually within an hour); after that the main card runs agy again."
    static let remove = "Remove deletes only the copy on this Mac. Google still accepts it until you revoke access in Google Account → third-party access, which also signs Antigravity out of that account."

    /// Popover order: general paragraphs first, then the toggle group last so
    /// "When on" / "this" follow the caption naming the switch.
    static let antigravityGeneral = [recipe, captureReads, remove]
    /// Paragraphs under the toggle caption; `autoOff` only while it is off.
    static func autoToggleGroup(autoCaptureOn: Bool) -> [String] {
        autoCaptureOn ? [autoOn] : [autoOn, autoOff]
    }
    /// Every Antigravity paragraph, for presence checks.
    static let antigravityDetails = antigravityGeneral + [autoOn, autoOff]
    static let antigravityConfirm = [confirmTitle, confirmBody, confirmOk, confirmCancel]
    /// Every string that has a translation; the product names `claude` and
    /// `antigravity` are intentionally the same in every language.
    static let translated = [page, moreDetails, claudeSummary, dshSummary, dshDetails, antigravitySummary,
                             autoToggle, piSummary, cursorSummary, removeNamed, folderMissing, addConfigDir,
                             addFolder, addPrompt, removeHelp, unreadableFolders, rejectedPaths, capturing,
                             captureCurrent, autoPaused, autoUnavailable]
        + claudeDetails + refusalSentences + piDetails + antigravityDetails + antigravityConfirm
        + antigravityMessages + CursorSync.Copy.all
}

/// ⓘ next to a section header; the popover holds the long explanation.
private struct SettingsInfoButton<Details: View>: View {
    /// Popover content width. Chosen for readable paragraphs; the wrapped
    /// height at this width has not been measured.
    private static var popoverWidth: CGFloat { 280 }

    /// Pause before a hover opens the popover, so sweeping the pointer across
    /// the page does not flash popovers. About the system tooltip delay.
    private static var hoverDelay: Duration { .milliseconds(400) }
    /// Grace after the pointer leaves the icon or the popover, so crossing the
    /// gap between them does not close it.
    private static var leaveGrace: Duration { .milliseconds(200) }
    let section: String

    let details: Details
    @State private var shown = false
    /// Opened by click or keyboard: stays open regardless of the pointer until
    /// Escape, a click elsewhere, or another click on the icon closes it.
    @State private var pinned = false
    @State private var overIcon = false
    @State private var overContent = false
    @State private var pending: Task<Void, Never>?

    var body: some View {
        // Hover opens the popover; a click, Space or Return pins it open, and
        // the same again closes it. Checked by hand in a preview build of
        // 394ada5a on macOS 27 (2026-11-01): a click pins a hover-opened
        // popover, a second click on the icon closes it, and Escape or a click
        // elsewhere closes it. Space / Return were not checked by hand.
        Button {
            pending?.cancel()
            if shown && pinned {
                shown = false
                pinned = false
            } else {
                pinned = true
                shown = true
            }
        } label: {
            Image(systemName: "info.circle")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(AccountsCopy.moreDetails.localized): \(section)")
        .onHover { inside in
            overIcon = inside
            pending?.cancel()
            if inside {
                guard !shown else { return }
                pending = Task {
                    try? await Task.sleep(for: Self.hoverDelay)
                    if !Task.isCancelled, overIcon { shown = true }
                }
            } else {
                scheduleClose()
            }
        }
        .onChange(of: shown) { _, now in
            if !now {
                pinned = false
                overContent = false
            }
        }
        .onDisappear {
            pending?.cancel()
            overIcon = false
            overContent = false
            shown = false
        }
        .popover(isPresented: $shown, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 8) { details }
                .frame(width: Self.popoverWidth, alignment: .leading)
                .padding(12)
                .onHover { inside in
                    overContent = inside
                    pending?.cancel()
                    if !inside { scheduleClose() }
                }
        }
    }

    private func scheduleClose() {
        pending = Task {
            try? await Task.sleep(for: Self.leaveGrace)
            if !Task.isCancelled, !pinned, !overIcon, !overContent { shown = false }
        }
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
