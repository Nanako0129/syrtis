import AppKit
import SwiftUI
import TokenBarCore

/// Popover root: view-switch row + lens router over a shared DashboardModel.
/// Per-client tabs join in a later phase.
struct PopoverView: View {
    private let routeMemory: StatusItemRouteMemory

    static func supportedTurnClients(_ ids: [String]) -> [String] {
        let supported = Set(["codex", "claude"])
        return ids.filter { supported.contains($0) }
    }

    init(routeMemory: StatusItemRouteMemory) {
        self.routeMemory = routeMemory
    }

    /// Owns the popover's size; the drag handle below writes its height.
    @EnvironmentObject private var chrome: PopoverChrome
    /// Height at the start of the active resize drag (global-space gesture).
    @State private var dragBase: CGFloat?

    // The popover's model owns the shared restore snapshot — its per-open
    // teardown/rebuild is exactly what the cache exists to speed up.
    @State private var model = DashboardModel(cachesSnapshot: true)
    @State private var tokensPerMin: Double?
    /// True while Cmd has been held alone for a beat — shows shortcut pins.
    @State private var cmdHeld = false
    /// Quick settings card under the header (#490), toggled by the header button.
    @State private var showQuickSettings = false
    @State private var keyMonitor: Any?
    @State private var flagsMonitor: Any?
    @State private var cmdHintTask: Task<Void, Never>?
    // Round 6 audit 2: matches the sibling `activeViewRaw` default below —
    // `ChartView.bars.rawValue`, not the bare "2d" literal this used to
    // duplicate.
    @AppStorage("tokenbar.chart.view") private var chartViewRaw = ChartView.bars.rawValue
    @AppStorage(ClientTray.activeViewKey) private var activeViewRaw = AppView.overview.rawValue
    @AppStorage("tokenbar.views.hidden") private var hiddenViewsRaw = ""
    /// The window card's own selection. Rebuilding on change is what makes the
    /// buttons feel like buttons — the quota poll is a minute apart.
    @AppStorage(WindowCardLoader.selectionKey) private var windowSelectionRaw = ""
    /// Bumped by the account pill row after it writes the per-client account
    /// preference. That preference is keyed per client, which a property wrapper
    /// cannot observe, so the write announces itself here instead.
    @State private var accountPickTick = 0
    @AppStorage("tokenbar.bridge.dismissed") private var bridgeDismissed = false
    /// "overview" or a client id. Persisted so the selection survives the
    /// popover's rootView teardown/rebuild cycle (StatusItemController swaps
    /// the live view for a placeholder on close).
    /// `--tab=<id>` preselects a client tab (debug/screenshot aid).
    @AppStorage(ClientTray.activeTabKey) private var activeTab =
        CommandLine.arguments
            .first(where: { $0.hasPrefix("--tab=") })
            .map { String($0.dropFirst("--tab=".count)) } ?? ClientTray.overviewTab
    // Observe the tab hidden/order keys so the popover reacts LIVE to a hide or
    // reorder made in the Settings window while it stays open — otherwise
    // `displayClients` (which reads UserDefaults) would only pick up the change
    // on the next reopen. Reading these raws in `displayClients` establishes the
    // body dependency; the reactive overload does the parsing.
    @AppStorage(ClientRegistry.tabHiddenKey) private var hiddenRaw = ""
    /// Observed, not read through `ClientRegistry.quotaExcludedClients()`: that
    /// helper reads `UserDefaults` directly, and a computed read is not a view
    /// dependency, so toggling a client's limits in Settings would leave the
    /// Overview summary naming it until something unrelated rebuilt the body.
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""
    @AppStorage(ClientRegistry.tabOrderKey) private var orderRaw = ""
    /// Observed so the Overview summary's projection follows the same setting
    /// the Agent-limits card obeys, live rather than on the next reopen.
    @AppStorage("tokenbar.limits.paceMode") private var paceModeRaw = PaceMode.historical.rawValue
    /// Observed, not read: a computed `UserDefaults` read is not a view
    /// dependency, so saving a classification in Settings would leave this
    /// split stale until something unrelated rebuilt the body. Same reasoning
    /// as `UsageAttributionBreakdownCard`, which learned it the hard way.
    @AppStorage(UsageAttribution.confirmedKey) private var attributionRaw = ""
    /// Observed so a scan-root change restarts the loads that depend on it —
    /// see the series task below.
    ///
    /// The GENERATION, not the persisted list. The list changes the instant
    /// Settings saves, which is before `ClaudeExtraRoots.apply` has handed the
    /// new set to the core, so a task keyed on it restarts early and publishes
    /// a load that scanned the old roots. The generation is bumped after the
    /// setter returns.
    @AppStorage(ClaudeExtraRoots.generationKey) private var extraRootsGeneration = 0

    /// Owns the attributed daily series. Mounted here for the reason its own
    /// doc comment gives: its timezone provenance is process-scoped, and the
    /// model expects to be `@State` on this view.
    @State private var series = AttributedSeriesModel()

    /// How many calendar days the trend covers. Two weeks reads as a rhythm
    /// without turning each column into a sliver at popover width.
    private static let trendDays = 14


    private var activeView: Binding<AppView> {
        Binding(
            get: { AppView(rawValue: activeViewRaw) ?? .overview },
            set: {
                activeViewRaw = $0.rawValue
                routeMemory.record(clientId: activeTab, view: $0.rawValue)
            })
    }

    private var clientTab: Binding<String> {
        Binding(
            get: { activeTab },
            set: { nextClient in
                guard nextClient != activeTab else { return }
                let route = routeMemory.switchClient(
                    from: activeTab, currentView: activeViewRaw, to: nextClient)
                activeTab = route.clientId
                activeViewRaw = route.view
            })
    }

    /// Lenses shown in the tab row — a hidden lens drops out the instant the
    /// user hides it in Settings. Reactive via `hiddenViewsRaw` so a live
    /// Settings toggle updates the row without reopening the popover.
    private var visibleViews: [AppView] {
        AppView.visible(hiddenRaw: hiddenViewsRaw)
    }

    /// This frame's actually-safe view — see `AppView.effective`.
    private var effectiveView: AppView {
        AppView.effective(activeView.wrappedValue, hiddenRaw: hiddenViewsRaw)
    }

    /// Available tabs come from both local usage and configured quota sources.
    private var presentTabClients: [String] {
        ClientRegistry.tabClients(
            present: model.stats?.presentClients ?? [],
            quotaIds: model.agentUsage?.configuredClientIds ?? [])
    }

    /// Visible tabs in saved order, shared by the tab row and navigation guards.
    private var displayClients: [String] {
        ClientRegistry.displayClients(
            present: presentTabClients, hiddenRaw: hiddenRaw, orderRaw: orderRaw)
    }

    private var displayUsageClients: [String] {
        ClientRegistry.displayClients(
            present: model.stats?.presentClients ?? [], hiddenRaw: hiddenRaw, orderRaw: orderRaw)
    }

    /// The open tab's two window-card roles (`WindowCardGate`): `card` is the
    /// client whose card, cycles and history are drawn (quota-only tabs
    /// included), `scan` the client whose local messages are scanned (only a
    /// tab with local records). The quota client list is computed here from the
    /// same inputs the model uses for `windowCardClients`, so the first body
    /// pass cannot see a stale empty list.
    private var quotaGate: (card: String?, scan: String?) {
        guard effectiveView == .quota else { return (nil, nil) }
        let present = model.stats?.presentClients ?? []
        // Year-independent: the card's scan covers quota history, not the
        // selected year (see `DashboardModel.localRecordClients`).
        return WindowCardGate.clients(
            tab: activeTab, presentClients: model.localRecordClients(),
            quotaClients: ClientRegistry.quotaClients(
                present: present, quotaIds: model.agentUsage?.configuredClientIds ?? [],
                tabHidden: ClientRegistry.parseIdSet(hiddenRaw), orderRaw: orderRaw),
            excluded: ClientRegistry.quotaExcludedClients(
                tabHidden: ClientRegistry.parseIdSet(hiddenRaw),
                limitsHidden: ClientRegistry.parseIdSet(limitsHiddenRaw)),
            // The same confirmed records the card's "Mine" fold uses.
            confirmed: UsageAttribution.parseRaw(attributionRaw).records)
    }
    private var quotaUsageClient: String? { quotaGate.scan }
    private var quotaCardClient: String? { quotaGate.card }

    /// Years shown in the picker: `knownYears` minus years in which ONLY hidden
    /// clients had activity. Best-effort — derivable only from an all-time
    /// payload (contributions span every year); when the payload is year-scoped
    /// (e.g. a snapshot restore before any all-time load has been seen) we keep
    /// the full known list (graceful degradation). Reactive via `hiddenRaw`; the
    /// empty-hidden fast path is byte-identical to the raw known list.
    private var visibleYears: [String] {
        let hidden = ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw))
        guard !hidden.isEmpty, model.year == nil,
              let contributions = model.payload?.contributions, !contributions.isEmpty
        else { return model.knownYears }
        let visible = UsageStats.yearsWithVisibleActivity(
            contributions: contributions, hidden: hidden)
        return model.knownYears.filter { visible.contains($0) }
    }

    /// The active tab's client slice, mirroring `lensContent`'s `clientIds`:
    /// the displayed (non-hidden) clients on Overview, or the single client on
    /// a client tab. Threaded into `ensureData` so the Hourly/Agents FFI fetch
    /// is scoped to the selection (accurate totals for shared hours/agents).
    private var lensClientIds: [String] {
        activeTab == ClientTray.overviewTab ? displayUsageClients : ClientRegistry.tabSlice(activeTab)
    }

    /// Daily/Monthly request turns only for visible canonical clients, keeping
    /// the incoming display order and never passing an empty all-client filter.
    private var turnClientIds: [String] {
        Self.supportedTurnClients(lensClientIds)
    }

    /// Daily/Monthly no longer request a lazy report — their turns ride the
    /// graph payload — so every lazy lens now keys on the full active slice.
    private var lazyClientIds: [String] { lensClientIds }

    private var quotaRefreshID: String {
        [windowSelectionRaw, activeTab, hiddenRaw, attributionRaw,
         WindowCardAccount.stored(clientId: activeTab) ?? "-", String(accountPickTick),
         quotaCardClient ?? "-", quotaUsageClient ?? "-",
         effectiveView.rawValue, String(extraRootsGeneration), limitsHiddenRaw, orderRaw,
         displayClients.joined(separator: ","), displayUsageClients.joined(separator: ",")]
            .joined(separator: "|")
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            if showQuickSettings {
                QuickSettingsCard()
                    .padding(.horizontal, 12)
                    .padding(.bottom, 8)
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
            if BridgeBuild.isActive && !bridgeDismissed {
                bridgeBanner
            }
            // Show the tabs row (which always starts with Overview) as soon as
            // there is client data, so Overview is visible alongside the clients.
            if !displayClients.isEmpty {
                DashboardTabs(
                    clients: displayClients,
                    presentClients: presentTabClients,
                    active: clientTab, kbdHints: cmdHeld)
                    .padding(.horizontal, 12)
                    .padding(.bottom, 8)
            }
            ViewSwitch(active: activeView, views: visibleViews)
                .padding(.horizontal, 12)
                .padding(.bottom, 10)
            PanelDivider()
            GeometryReader { viewport in
                ScrollView {
                    content
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(OverlayScrollerEnforcer())
                }
                // Live global frame for tooltip clamp. While the height
                // handle is dragged, publish nil so placement falls back to
                // the source card only — avoids every-pixel env churn AND a
                // frozen pre-drag rect that would leave a fake dodge after
                // the user hovers Models/Usage immediately post-resize.
                .environment(
                    \.popoverScrollViewport,
                    dragBase == nil ? viewport.frame(in: .global) : nil)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()
                // A new ScrollView per tab and view: the incoming lens starts
                // at the top while the outgoing one fades out where it was.
                // Scrolling one shared ScrollView back up would show the old
                // lens scroll before the crossfade, and delay it.
                .id(lensKey)
                .transition(.opacity.combined(with: .scale(scale: 0.985, anchor: .top)))
            }
            PanelDivider()
            footer
        }
        // AppKit owns the live drag size. Filling the hosting view avoids
        // publishing a new environment-object height — and rebuilding this
        // entire view tree — for every pointer event.
        .frame(width: chrome.width)
        .frame(maxHeight: .infinity)
        .animation(.easeOut(duration: 0.16), value: activeViewRaw)
        .animation(.easeOut(duration: 0.16), value: activeTab)
        .background(PopoverBackdrop().ignoresSafeArea())
        .overlay(alignment: .bottom) { resizeHandle }
        .task { await model.load() }
        // Keyed on the year AND the client slice: a year switch, a tab switch,
        // or a hide toggle must re-fetch the active lazy lens (Hourly/Agents)
        // for the new slice. Without the year in the key, switching years while
        // parked on Hourly/Agents would keep showing the old year (reload()'s
        // lazy re-fetch only refreshes an already-loaded lens, so a lens still
        // nil from the reopen would never reload); without the slice, switching
        // client tabs would serve the previous tab's FFI-filtered totals.
        // Keyed on the raw activeViewRaw, not effectiveView: intentional. If a
        // now-hidden lazy lens (Hourly/Agents) is still the persisted active
        // view for one frame, this fires an ensureData fetch for it — but
        // resetViewIfHidden() immediately rewrites activeViewRaw to
        // "overview" (same onChange pass), which changes this task's id and
        // cancels the in-flight fetch before it commits. Self-correcting;
        // switching to effectiveView here isn't needed for correctness.
        .task(id: "\(activeViewRaw)|\(model.year ?? "")|\(lazyClientIds.joined(separator: ","))") {
            await model.ensureData(for: activeView.wrappedValue, clients: lazyClientIds)
        }
        // Model report, keyed on the COMMITTED slice rather than the requested
        // one. Before any payload lands the key is empty, so this fires as a
        // no-op; it re-fires the moment apply() commits, which is what keeps
        // the model scan off the graph's critical path instead of racing it
        // for the same Rayon pool, and again whenever a refresh moves the
        // payload. Keying on `model.year` instead looked equivalent but was
        // not: that changes at the moment of intent, so a slice whose payload
        // shares the previous generation — an all-years and a current-year
        // view are both dated today — never re-fired and never fetched.
        .task(id: "\(activeViewRaw)|\(model.committedSliceKey)") {
            await model.ensureModelData(for: activeView.wrappedValue)
        }
        // Auto-clear a year filter scoped to a year only hidden clients used —
        // re-checked on a live hide toggle (hiddenRaw) and on each payload load
        // (generatedAt). The model no-ops unless the scoped payload has no
        // visible activity, and clears to All years via setYear.
        .task(id: "\(hiddenRaw)|\(model.payload?.meta.generatedAt ?? "")") {
            await model.clearYearIfHiddenOnly(hidden: ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw)))
        }
        // Keyed on the hidden raw so a hide toggle restarts the loop and
        // re-fetches the filtered rate immediately (badge would otherwise lag
        // ≤10s). The loop fetches first, then sleeps.
        .task(id: hiddenRaw) { await pollTokensPerMin() }
        // Keyed on the account generation, not unkeyed. The quota cards come
        // from this loop, and the loop sleeps 60s between fetches — so removing
        // a Claude account cleared the registry immediately while its card sat
        // on screen for up to a minute afterwards, describing an account the
        // engine no longer fetches. Restarting cancels the in-flight fetch,
        // which is correct rather than wasteful: that request was issued for
        // the previous account set and its answer is about accounts that are
        // no longer configured.
        //
        // Same shape as the Discord value gates in `AppDelegate`: published
        // state and the thing that authorises it have to move in the same
        // turn, or one outlives the other.
        .task(id: extraRootsGeneration) { await model.pollAgentUsage() }
        // The card's own trigger, deliberately not inside the quota poll. The
        // union range must cover every displayed client, not just the open tab,
        // so one scan can serve a later switch without rescanning.
        // `displayClients` is in the identity because the closure READS it, and
        // it derives from `stats.presentClients` — graph data that arrives
        // after this view first appears. Without it the first firing captures
        // an empty client list, and since nothing else in the identity moves,
        // the task never runs again: opening straight onto an agent tab left
        // the card permanently absent until you switched tabs. Same trap the
        // `.onChange` below documents for `presentClients`.
        // Keyed on `attributionRaw` too: the window card, the history rows and
        // the equivalence are all folded through the declaration set, and a
        // classification saved in Settings would otherwise reach the daily
        // chart at once (its own task IS keyed on it) while these three kept
        // reporting the previous split until the next minute poll.
        // And on the scan-root generation, for the reason the series task
        // below is: `invalidateScanDerivedCaches` clears the static union scan
        // but not the `windowCards`, `quotaHistory` and `quotaEquivalences`
        // this model has already published, and nothing else in this identity
        // moves when a root is added or removed — so every visible quota
        // surface kept drawing the old root set until a poll happened to call
        // `refreshWindowUsage` again, up to a minute later.
        .task(id: quotaRefreshID) {
            model.configureQuotaVisibility(
                tabHidden: ClientRegistry.parseIdSet(hiddenRaw),
                limitsHidden: ClientRegistry.parseIdSet(limitsHiddenRaw), orderRaw: orderRaw)
            // The scan follows what is on screen; the curves do not. See
            // `windowUsageClient`.
            // Gated on the lens too: the window card and its history live only
            // here now, so no other lens can make the app pay for a scan.
            model.windowUsageClient = quotaUsageClient
            // After the scan client (setting it moves the card client too): a
            // quota-only tab has a card and no scan.
            model.cardClient = quotaCardClient
            // The all-agent Quota lens is the only surface that wants the
            // equivalence scan, and only while it is on screen.
            model.quotaLensAllAgents =
                effectiveView == .quota && activeTab == ClientTray.overviewTab
            model.refreshWindowQuotaHalves()   // synchronous: lands this frame
            await model.refreshWindowUsage()
        }
        .task { await model.pollTrace() }
        .task { await model.pollGraph() }
        // Keyed on the declarations too: re-splitting the stack is the whole
        // point of the card, and a classification saved in Settings has to
        // reach it without waiting for a poll.
        // Keyed on the scan roots as well as the declarations. Clearing the
        // static cache and superseding in-flight loads does not touch the
        // `points` this model has already published, and this task had no other
        // reason to restart — so a root added or removed while the popover
        // stayed open left the trend drawing the old set indefinitely. Exactly
        // the shape the timezone case below is written for, which is why this
        // is declarative rather than a second notification: the raw list is
        // already persisted, so keying on it restarts the load by construction.
        .task(id: "\(attributionRaw)|\(extraRootsGeneration)") {
            await series.load(
                source: UsageDataSources.current,
                confirmed: UsageAttribution.parseRaw(attributionRaw).records)
        }
        // A transition invalidates the model's provenance and its cached rows,
        // but the `points` it already published are untouched and this task is
        // keyed on the declarations alone, so nothing restarts it. The trend
        // chart would keep drawing day buckets and axis labels from the zone
        // the user just left until the popover was rebuilt.
        .onReceive(NotificationCenter.default.publisher(
            for: .NSSystemTimeZoneDidChange)) { _ in
            // The grids bucket by the current zone too, and only
            // `refreshWindowQuotaHalves` rebuilds them — so without this the
            // weekday cells and the observed-day count stay in the zone the
            // user left until a later successful quota poll, or for ever if
            // those keep failing. Teaching this handler about one of the two
            // timezone-dependent folds was the same half-applied rule as the
            // rest of this round.
            model.refreshWindowQuotaHalves()
            Task {
                await series.load(
                    source: UsageDataSources.current,
                    confirmed: UsageAttribution.parseRaw(attributionRaw).records)
            }
        }
        .onAppear {
            installKeyMonitors()
            // `--tab=` must win even after activeTab is persisted (@AppStorage
            // only reads the launch default when the key is absent), so the
            // screenshot/debug flag keeps preselecting the tab on every launch.
            if let tabArg = CommandLine.arguments
                .first(where: { $0.hasPrefix("--tab=") })
                .map({ String($0.dropFirst("--tab=".count)) }) {
                let route = routeMemory.switchClient(
                    from: activeTab, currentView: activeViewRaw, to: tabArg)
                activeTab = route.clientId
                activeViewRaw = route.view
            }
            routeMemory.record(clientId: activeTab, view: activeViewRaw)
        }
        .onDisappear { removeKeyMonitors() }
        // Reset a stale persisted tab whenever the displayed client set changes,
        // so a saved client id that no longer exists (or that the user just hid)
        // can't strand the dashboard on an empty slice with no visible tab row
        // to return to Overview. Attached to the always-present root, not the
        // tab row (which is hidden when only one client is present). TWO signals
        // are needed:
        //   - presentClients (initial: true): the LOAD signal. When every client
        //     is hidden, displayClients stays [] across the nil->loaded
        //     transition, so a displayClients onChange would never fire — but
        //     presentClients still deltas on load, catching a persisted-then-
        //     hidden tab across a relaunch.
        //   - displayClients: the LIVE-hide signal. Hiding the active tab in the
        //     Settings window leaves it in presentClients (so the load onChange
        //     does NOT fire), but now that hiddenRaw/orderRaw are observed,
        //     displayClients is reactive state and deltas the instant the toggle
        //     lands. Without this, a live hide would strand the slice until
        //     reopen.
        // The graph task is not keyed on the generation and `pollGraph` sleeps
        // before fetching, so Overview and the model/hourly/agents lenses would
        // keep drawing the old root set for up to a minute after an apply. This
        // also supersedes an old-root fetch still in flight: `gatedGraph` bumps
        // `graphFetchToken`, and a fetch that no longer owns it cannot commit.
        //
        // Not `initial: true`: the first value is the launch state, and
        // `load()` already covers t=0.
        .onChange(of: extraRootsGeneration) { _, _ in
            Task { await model.reloadForRootChange() }
        }
        .onChange(of: model.stats?.presentClients, initial: true) { _, _ in
            resetTabIfHidden()
        }
        .onChange(of: displayClients) { _, _ in
            resetTabIfHidden()
        }
        .onChange(of: model.agentUsageAttempted) { _, _ in
            resetTabIfHidden()
        }
        .onChange(of: hiddenViewsRaw, initial: true) { _, _ in
            resetViewIfHidden()
        }
    }

    /// Fall back to Overview if the active client tab is no longer displayed
    /// (hidden or gone). The stats-nil guard skips the pre-load fire so a
    /// persisted tab isn't reset before data arrives (defeating
    /// tokenbar.activeTab's cross-launch memory). Membership is judged against
    /// displayClients so hiding the active tab — which leaves it in
    /// presentClients — still falls back.
    private func resetTabIfHidden() {
        // Wait for both discoveries before discarding a saved quota-only tab.
        // An explicit hide still takes effect immediately.
        guard ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw)).contains(activeTab)
            || (model.stats != nil && model.agentUsageAttempted) else { return }
        if activeTab != ClientTray.overviewTab, !displayClients.contains(activeTab) {
            clientTab.wrappedValue = ClientTray.overviewTab
        }
    }

    /// Fall back to Overview if the active lens just got hidden in Settings —
    /// the tab-row analog of `resetTabIfHidden()`.
    private func resetViewIfHidden() {
        if effectiveView != activeView.wrappedValue {
            activeView.wrappedValue = effectiveView
        }
    }

    // MARK: - Sections

    private var header: some View {
        HStack {
            BrandMark()
                .frame(width: 17, height: 17)
            Text("Σύρτις")
                .font(.syrtisWordmark(size: 15))
                .foregroundStyle(Color.syrtisAccent)
            Spacer()
            liveRateBadge
            yearMenu
            if GlassTintSlider.glassAvailable {
                quickSettingsButton
            }
            refreshButton
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private static let restoredAgeFormatter: RelativeDateTimeFormatter = {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .abbreviated
        return formatter
    }()

    /// Year filter for every lens — the Tauri HeaderBar's year select. "All"
    /// (nil) is the native default; concrete years come from the payloads
    /// seen so far.
    @ViewBuilder private var yearMenu: some View {
        let years = visibleYears
        if !years.isEmpty {
            Menu {
                Picker("Year", selection: Binding(
                    get: { model.year ?? "" },
                    set: { value in
                        Task { await model.setYear(value.isEmpty ? nil : value) }
                    }
                )) {
                    Text("All years").tag("")
                    ForEach(years, id: \.self) { year in
                        Text(year).tag(year)
                    }
                }
                .pickerStyle(.inline)
                .labelsHidden()
            } label: {
                Text((model.year ?? "All").localized)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.visible)
            .fixedSize()
            .help("Filter usage by year")
        }
    }

    /// Shown only on the final beta build (1.0 on the .beta id): one tap runs
    /// the cask install that graduates this install to the release app.
    private var bridgeBanner: some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.up.forward.app.fill")
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 1) {
                Text("You're on the beta build")
                    .font(.caption.weight(.semibold))
                Text("Switch to the Syrtis 1.0 release — keeps your data")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button("Switch") { BridgeBuild.switchToRelease() }
                .controlSize(.small)
                .buttonStyle(.borderedProminent)
            Button {
                bridgeDismissed = true
            } label: {
                Image(systemName: "xmark")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(Color.primary.opacity(0.06))
    }

    /// Manual refresh (also ⌘R), and the freshness signal for every other kind
    /// of refresh.
    ///
    /// One control rather than a second indicator beside it: this corner already
    /// renders a spinner for a manual refresh, so a background refresh reusing
    /// it needs no new element and no new place for the eye to learn.
    ///
    /// Both kinds disable the button, so both render identically. An earlier
    /// version left background refreshes clickable to allow pre-empting one by
    /// hand; that made the same spinner appear at two different weights, since
    /// a disabled plain button dims its label. Pressing Refresh during a scan
    /// only supersedes it with another full scan anyway, so the consistency is
    /// worth more than the interruption.
    private var quickSettingsButton: some View {
        Button {
            withAnimation(.easeOut(duration: 0.15)) { showQuickSettings.toggle() }
        } label: {
            Image(systemName: "slider.horizontal.3")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(showQuickSettings ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
                .frame(width: 16, height: 16)
        }
        .buttonStyle(.plain)
        .help("Quick settings".localized)
        .accessibilityLabel("Quick settings".localized)
    }

    private var refreshButton: some View {
        Button {
            Task { await model.refresh() }
        } label: {
            if model.refreshing || backgroundRefreshRunning {
                ProgressView()
                    .controlSize(.small)
                    .frame(width: 16, height: 16)
            } else {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 11, weight: .medium))
                    // A restored dashboard whose refresh FAILED is the one state
                    // where the data is stale and nothing is running to fix it.
                    // Tinting the existing glyph says so without adding a
                    // control; the age itself is in the tooltip.
                    .foregroundStyle(showingStaleRestore ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                    .frame(width: 16, height: 16)
            }
        }
        .buttonStyle(.plain)
        .disabled(refreshDisabled)
        .help(refreshHelp)
        // The SAME condition that drives the spinner and the disabled state.
        // Keying the label on `backgroundRefreshRunning` alone announced
        // "Refresh usage data" during a manual refresh, when the control is
        // spinning and disabled — the opposite of what a screen reader user
        // would be told by the visual state.
        .accessibilityLabel(
            model.refreshing || backgroundRefreshRunning
                ? "Updating usage data".localized : "Refresh usage data".localized)
    }

    /// One source for the button's disabled state and the ⌘R shortcut, so the
    /// keyboard path cannot outlive a restriction the visible control shows.
    private var refreshDisabled: Bool {
        model.refreshing || backgroundRefreshRunning
    }

    /// A graph fetch that the user did not start. `refreshing` already owns the
    /// manual kind, so including it here would just double-count it.
    private var backgroundRefreshRunning: Bool {
        guard let refresh = model.backgroundRefresh else { return false }
        return refresh.kind != .manual
    }

    /// Restored data still on screen after the live refresh failed.
    private var showingStaleRestore: Bool {
        model.restoredSnapshot?.failed == true
    }

    /// Built from localized format strings rather than interpolation.
    ///
    /// `.help` treats a String LITERAL as a `LocalizedStringKey` and translates
    /// it for free; a computed `String` bypasses that entirely. Moving the
    /// tooltip behind this property therefore silently turned it English for
    /// every zh-Hant user until the keys below were added.
    private var refreshHelp: String {
        guard let restored = model.restoredSnapshot else {
            return "Refresh usage data (⌘R)".localized
        }
        let age = Self.restoredAgeFormatter.localizedString(
            for: restored.savedAt, relativeTo: Date())
        return "Refresh usage data (⌘R) — showing data from %@".localized(age)
    }

    private var liveRateBadge: some View {
        HStack(spacing: 4) {
            activityLED
            Text(tokensPerMin.map { "\(Format.compactTokens(Int64($0.rounded()))) tok/min" } ?? "— tok/min")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
        }
    }

    /// Network-LED behavior: steady dim gray when idle, and when tokens are
    /// flowing, a green light that flickers irregularly like a router's
    /// activity light — mostly lit, with brief pseudo-random off-blinks,
    /// denser at higher rates. The flicker runs in Core Animation
    /// (`ActivityLED`) so it does not re-render the popover every frame.
    @ViewBuilder private var activityLED: some View {
        let rate = tokensPerMin ?? 0
        if rate > 0 {
            // Blink-off chance grows with the rate: ~25% near idle,
            // ~45% at 1M tok/min — busier traffic, busier light.
            ActivityLED(offChance: 25 + min(20, Int(rate / 50_000)))
                .frame(width: 6, height: 6)
        } else {
            Circle()
                .fill(.secondary.opacity(0.4))
                .frame(width: 6, height: 6)
        }
    }

    @ViewBuilder private var content: some View {
        switch model.phase {
        case .loading:
            HStack(spacing: 8) {
                ProgressView()
                    .controlSize(.small)
                Text("Loading usage…")
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, minHeight: 120)
        case let .failed(message):
            Label(message, systemImage: "exclamationmark.triangle")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, minHeight: 120)
        case .ready:
            lens
        }
    }

    /// Identity of the visible lens. The content ScrollView is keyed on it,
    /// so switching either the tab or the view swaps the whole scroll view,
    /// and the crossfade transition lives there.
    private var lensKey: String { "\(activeTab)|\(activeViewRaw)" }

    /// Lens router. The client tab picks *which* data (clientIds slice), the
    /// view switch picks *how* it is broken down; the two compose.
    @ViewBuilder private var lens: some View {
        lensContent
    }

    @ViewBuilder private var lensContent: some View {
        if let payload = model.payload, let stats = model.stats {
            // Treat a just-hidden (or unknown) active tab as Overview for THIS
            // frame's slice, so the hidden client's slice never renders even for
            // the single body pass before `resetTabIfHidden()` fixes the
            // persisted `activeTab`. Use the reactive `displayClients` (observes
            // the hidden/order raws) so a live hide re-derives the slice.
            let singleClient = (activeTab != ClientTray.overviewTab && displayClients.contains(activeTab))
                ? activeTab : nil
            let clientIds = singleClient.map(ClientRegistry.tabSlice) ?? displayUsageClients
            // Every displayed number must exclude hidden clients — including the
            // Overview aggregates. The model reuses the precomputed full `stats`
            // for the all-present slice and memoizes the hidden/single-client
            // slice, so this hot path (re-evals every ~10s trace poll) doesn't
            // re-aggregate UsageStats on every body eval.
            let activeStats = model.stats(selecting: Set(clientIds)) ?? stats
            let turnClientIds = Self.supportedTurnClients(clientIds)
            switch effectiveView {
            case .overview:
                VStack(spacing: 0) {  // onboarding containers carry their own gap
                    // Only on the global tab: a single client's usage is
                    // already known whose subscription it belongs to, so
                    // there is nothing here for that tab to onboard.
                    if activeTab == ClientTray.overviewTab {
                        OnboardingSetupCards(
                            // Every present agent, hidden tabs included: the
                            // card asks which ones get a tab.
                            presentClients: presentTabClients,
                            modelReport: model.modelReport, agentUsage: model.agentUsage)
                    }
                    OverviewView(
                        payload: payload, clientIds: clientIds, stats: activeStats,
                        modelReport: model.modelReport, modelLoading: model.modelLoading,
                        colors: model.colors,
                        trace: model.trace,
                        singleClient: singleClient,
                        hasLocalUsage: clientIds.contains { model.stats?.presentClients.contains($0) == true },
                        year: model.year,
                        hidden: ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw)),
                        // The user's own pace mode, not the fold's default. Leaving
                        // it out meant the summary always projected Historically
                        // while the card beside it obeyed the setting — and with
                        // pace off, the card hid its marker while the summary kept
                        // naming a fastest burner. `compute` returns nil for `.off`,
                        // so passing the real mode suppresses the line too.
                        // The same union `ClientRegistry.quotaExcludedClients()`
                        // forms for the tray, built from observed values here. `QuotaSummaryFold` documents
                        // that it reuses `QuotaResolver` so this sentence and the
                        // menu bar can never name different subscriptions — but the
                        // shared function was being handed different arguments, so
                        // a client hidden only from Agent limits was excluded by
                        // the tray and named by this line. Since the limits card
                        // stopped drawing that client's row, the sentence pointed
                        // at something no longer below it.
                        quotaSummary: QuotaSummaryFold.build(
                            payload: model.agentUsage,
                            excluding: ClientRegistry.quotaExcludedClients(
                                tabHidden: ClientRegistry.parseIdSet(hiddenRaw),
                                limitsHidden: ClientRegistry.parseIdSet(limitsHiddenRaw)),
                            paceMode: PaceMode(rawValue: paceModeRaw) ?? .historical),
                        usageAttempted: model.agentUsageAttempted,
                        // The FOURTH statement of this gate, and the one that made
                        // the previous fix inert: the Overview lens is fed here, so
                        // removing the gate at the Quota lens' call site below and
                        // inside `OverviewView` left this one still handing a client
                        // tab an empty map. The rule was written in four places and
                        // reconciled in three.
                        windowCurves: model.windowCurves,
                        agentUsage: model.agentUsage)
                }
            case .quota:
                VStack(spacing: 0) {  // onboarding containers carry their own gap
                    // On every tab: this lens is where the window history reads
                    // 0 / $0.00 until usage is attributed, so the offer has to be
                    // here, not only on the global Overview.
                    AttributionOnboardingCardView(
                        modelReport: model.modelReport, agentUsage: model.agentUsage)
                    QuotaView(
                        singleClient: singleClient, clientIds: clientIds,
                        trace: model.trace, agentUsage: model.agentUsage,
                        usageAttempted: model.agentUsageAttempted,
                        // Scoped to the tab being drawn: the history card belongs to
                        // one client, and a failure recorded against another is not
                        // an answer about this one.
                        scanFailed: singleClient.map(model.windowScanFailed(for:)) ?? false,
                        curveUnreadable: model.quotaCurveUnreadable,
                        // Both lenses now. The gate here used to pass `[:]` on a
                        // client tab, because `@Observable` tracks per property and
                        // reading this on a lens that drew no sparkline would make
                        // every `refreshWindowQuotaHalves()` write invalidate this
                        // body for nothing. That lens draws one: its Agent-limits
                        // card has the same recent-consumption indicator as the
                        // all-agent one, and the indicator is computed from these
                        // curves — so the gate was not saving work, it was blanking
                        // a feature.
                        windowCurves: model.windowCurves,
                        windowCard: quotaCardClient.flatMap { model.windowCard(for: $0) },
                        accountContext: quotaCardClient.flatMap { model.cardAccountContext(for: $0) },
                        onSelectAccount: { account in
                            if let client = quotaCardClient {
                                UserDefaults.standard.set(
                                    account ?? "", forKey: WindowCardAccount.prefKey(clientId: client))
                                accountPickTick &+= 1
                            }
                        },
                        quotaCycles: quotaCardClient.map { model.cardHistory(for: $0).cycles } ?? model.quotaCycles,
                        quotaHistory: quotaCardClient.map { model.cardHistory(for: $0).rows } ?? model.quotaHistory,
                        historyPending: quotaCardClient.map { model.cardHistory(for: $0).pending } ?? false,
                        colors: model.colors,
                        // Folded from the series model rather than from the raw
                        // payload: that model refuses to publish day buckets built
                        // under a timezone it cannot vouch for, and calling the
                        // fold directly would silently skip that check.
                        windowSummaries: model.quotaWindowSummaries,
                        stripUnreadableClients: model.quotaUnreadableClients,
                        heatmaps: model.quotaHeatmaps,
                        heatmapWindows: model.quotaHeatmapWindows,
                        equivalences: model.quotaEquivalences,
                        trend: series.points.map {
                            SubscriptionTrendFold.build(
                                points: $0, today: Format.todayKey(),
                                days: Self.trendDays)
                        })
                }
            case .models:
                ModelsView(
                    report: model.modelReport, clientIds: clientIds, colors: model.colors,
                    loading: model.modelLoading)
            case .daily:
                DailyView(
                    payload: payload, clientIds: clientIds,
                    turnClientIds: turnClientIds,
                    colors: model.colors,
                    onExpand: { Task { await model.ensureModelColors() } })
            case .monthly:
                MonthlyView(
                    payload: payload, clientIds: clientIds,
                    turnClientIds: turnClientIds,
                    colors: model.colors,
                    onExpand: { Task { await model.ensureModelColors() } })
            case .hourly:
                HourlyView(report: model.hourlyReport(for: clientIds), clientIds: clientIds)
            case .stats:
                StatsView(
                    payload: payload, clientIds: clientIds, stats: activeStats,
                    modelReport: model.modelReport, modelYear: model.modelYear,
                    colors: model.colors,
                    year: model.year, singleClient: singleClient,
                    reportLoading: model.modelLoading)
            case .agents:
                AgentsView(report: model.agents, clientIds: clientIds)
            }
        }
    }

    private var footer: some View {
        HStack {
            Text(effectiveView.label)
                .font(.caption)
                .foregroundStyle(.tertiary)
            Spacer()
            if let version = UpdaterService.shared.availableVersion {
                Button {
                    UpdaterService.shared.checkForUpdates()
                } label: {
                    Label("Update \(version)", systemImage: "arrow.down.circle.fill")
                        .font(.caption.weight(.medium))
                }
                .controlSize(.small)
                .tint(.accentColor)
                .modifier(PanelFooterButton())
                .help("A new version is ready — click to install")
            }
            Button {
                openSettingsWindow(from: NSApp.keyWindow)
            } label: {
                Image(systemName: "gearshape")
            }
            .controlSize(.small)
            .modifier(PanelFooterButton())
            .help("Settings")
            Button("Quit") {
                NSApp.terminate(nil)
            }
            .controlSize(.small)
            .modifier(PanelFooterButton())
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    // MARK: - Resize handle

    /// Bottom-edge grabber: drag to set the popover height. Lives in the empty
    /// center of the footer strip (the buttons hug the edges) so it never
    /// steals their clicks. Global coordinate space keeps the drag stable as
    /// the popover grows under the pointer.
    private var resizeHandle: some View {
        Capsule()
            .fill(Color.primary.opacity(0.18))
            .frame(width: 36, height: 4)
            .frame(width: 90, height: 14) // larger hit target
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 1, coordinateSpace: .global)
                    .onChanged { value in
                        let base = dragBase ?? chrome.height
                        dragBase = base
                        chrome.setHeight(
                            base + value.translation.height, persist: false, live: true)
                    }
                    .onEnded { value in
                        let base = dragBase ?? chrome.height
                        chrome.setHeight(
                            base + value.translation.height, persist: true, live: false)
                        dragBase = nil
                    }
            )
            .hoverCursor(.resizeUpDown)
            .help("Drag to resize the popover height")
    }

    // MARK: - Keyboard shortcuts

    /// The web app's Cmd shortcuts (App.tsx onKeyDown), as local NSEvent
    /// monitors scoped to the popover's key window: ⌘1-9 tabs, ⌘[/⌘] cycle,
    /// ⌘, settings, ⌘R refresh, ⌘G cycle chart view (Bars/Heatmap/3D), ⌘W/Esc
    /// close, ⌘Q quit. Holding Cmd
    /// alone for 400ms reveals the tab pins (system chords like ⌘⇧4 don't).
    private func installKeyMonitors() {
        guard keyMonitor == nil else { return }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            handleKeyDown(event) ? nil : event
        }
        flagsMonitor = NSEvent.addLocalMonitorForEvents(matching: .flagsChanged) { event in
            handleFlagsChanged(event)
            return event
        }
    }

    private func removeKeyMonitors() {
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        if let flagsMonitor { NSEvent.removeMonitor(flagsMonitor) }
        keyMonitor = nil
        flagsMonitor = nil
        cmdHintTask?.cancel()
        cmdHeld = false
    }

    /// Settings live in their own window now; the transient popover closes
    /// itself on the way (programmatic window swaps don't count as the
    /// outside click that would normally dismiss it). Present on the NEXT
    /// runloop turn: showing the window in the same turn as the popover's
    /// animated close puts both vibrant windows in one CoreAnimation
    /// transaction and the native switch thumbs lose their first frame (blue
    /// track, no knob) until a later window-level invalidation. Launching with
    /// `--settings` has no popover and never showed the artifact.
    private func openSettingsWindow(from popoverWindow: NSWindow?) {
        popoverWindow?.performClose(nil)
        DispatchQueue.main.async {
            SettingsWindowController.shared.show()
        }
    }

    /// Returns true when the event was consumed.
    private func handleKeyDown(_ event: NSEvent) -> Bool {
        if event.keyCode == 53 { // Esc closes the popover
            event.window?.performClose(nil)
            return true
        }
        let mods = event.modifierFlags.intersection([.command, .shift, .option, .control])
        guard mods == .command, let chars = event.charactersIgnoringModifiers?.lowercased()
        else { return false }

        let tabs = [ClientTray.overviewTab] + displayClients
        switch chars {
        case "1", "2", "3", "4", "5", "6", "7", "8", "9":
            let index = Int(chars)! - 1
            guard index < tabs.count else { return true }
            clientTab.wrappedValue = tabs[index]
        case "[", "]":
            let current = tabs.firstIndex(of: activeTab) ?? 0
            let step = chars == "]" ? 1 : tabs.count - 1
            clientTab.wrappedValue = tabs[(current + step) % tabs.count]
        case ",":
            openSettingsWindow(from: event.window)
        case "w":
            event.window?.performClose(nil)
        case "q":
            NSApp.terminate(nil)
        case "r":
            // The same condition the button uses. Gating only the button left
            // the shortcut able to launch a forced scan alongside a running
            // background one: the ownership token stops the older result from
            // committing, but it does not cancel the FFI work, so two full
            // scans contend on the bounded pool while the control says it is
            // disabled and updating.
            //
            // Gated here rather than inside `DashboardModel.refresh()` on
            // purpose. Overlapping fetches are a capability the model must keep
            // — several regressions drive `load()` and `refresh()` concurrently
            // to prove the supersession rules — so the restriction belongs to
            // the user-facing control, not to the API.
            guard !refreshDisabled else { return true }
            Task { await model.refresh() }
        case "g":
            // Round 6, FIX 1: cycle order lives on `ChartView` (bars →
            // heatmap → 3D → bars) — not hardcoded here — so this handler
            // never again silently drops a view out of the ⌘G cycle when a
            // new one is added.
            chartViewRaw = ChartView(raw: chartViewRaw).next.rawValue
        default:
            return false
        }
        return true
    }

    private func handleFlagsChanged(_ event: NSEvent) {
        let mods = event.modifierFlags.intersection([.command, .shift, .option, .control])
        if mods == .command {
            guard cmdHintTask == nil, !cmdHeld else { return }
            cmdHintTask = Task {
                try? await Task.sleep(for: .milliseconds(400))
                if !Task.isCancelled { cmdHeld = true }
                cmdHintTask = nil
            }
        } else {
            cmdHintTask?.cancel()
            cmdHintTask = nil
            cmdHeld = false
        }
    }

    // MARK: - Live rate

    /// Poll the live rate every 10s while the popover content is on screen;
    /// `.task` cancels this loop when the popover closes.
    private func pollTokensPerMin() async {
        while !Task.isCancelled {
            let rate = await model.tokensPerMin()
            if Task.isCancelled { break }
            tokensPerMin = rate
            try? await Task.sleep(for: .seconds(10))
        }
    }
}
