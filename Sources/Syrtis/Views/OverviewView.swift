import SwiftUI
import TokenBarCore

/// What Overview shows of the selected devices' combined pages (DR-2a): the
/// usage cards (chart, models, streaks) come from these, never from this
/// Mac's own payload; the quota summary, the Agent-limits card, the trace and
/// "Today" stay this Mac's. A page that has not arrived is nil and its cards
/// show a loading line, not this Mac's numbers in its place.
struct OverviewMerge {
    /// Devices combined (this Mac and the chosen ones).
    let deviceCount: Int
    /// The graph overlay's own payload and stats, for the clients shown.
    let graph: (payload: UsagePayload, stats: UsageStats)?
    let models: ModelReport?
    let colors: ModelColorMap
    /// The clients the combined pages show (a client only a remote device used
    /// included).
    let clientIds: [String]
    /// The same for the models page.
    let modelClientIds: [String]
    /// The graph or models refresh failed with nothing to show.
    let graphFailed: Bool
    let modelsFailed: Bool
}

/// The usage stack. Leads with the chart, then the live-session trace and the
/// model breakdown.
///
/// Quota lives in its own lens as of 2026-08-17 — the window card and the
/// Agent-limits card answer one question at two scales, and keeping them here
/// while the window history sat elsewhere split one subject across two places.
struct OverviewView: View {
    let payload: UsagePayload
    /// The active tab's client slice (all present clients on Overview).
    let clientIds: [String]
    let stats: UsageStats
    let modelReport: ModelReport?
    /// Forwarded to ModelBreakdownCard; see its `loading` doc.
    var modelLoading = false
    let colors: ModelColorMap
    let trace: [TraceBucket]
    /// Set when this view shows a single client's slice.
    var singleClient: String?
    var hasLocalUsage = true
    /// Dashboard year filter (nil = all time), forwarded to the chart card.
    var year: String?
    /// The user's tab-hidden set, passed in from the observing parent
    /// (PopoverView) so the dependency is explicit rather than an imperative
    /// `ClientRegistry.hiddenClients()` read in this body.
    var hidden: Set<String> = []
    /// Cross-subscription quota position, folded by the model. Nil means no
    /// subscription reported a usable window.
    var quotaSummary: QuotaSummary?
    /// Whether the first quota fetch has settled, so a nil summary can read as
    /// "nothing to report" rather than "still asking".
    var usageAttempted = true
    /// Quota readings per `"<clientId>|<cardId>"`, for the Agent-limits card's
    /// sparkline and recent-consumption indicator. Both tabs: the client tab
    /// draws the same card, and an empty map here silently blanks the indicator
    /// rather than saving anything.
    var windowCurves: [String: [QuotaSample]] = [:]
    var agentUsage: AgentUsagePayload?
    /// Set while devices other than this Mac are selected.
    var merged: OverviewMerge?

    @AppStorage(OverviewCard.hiddenKey) private var hiddenCardsRaw = ""
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true

    var body: some View {
        VStack(spacing: 12) {
            // Quota cards moved to the Quota lens (2026-08-17): the window card
            // and the Agent-limits card answer the same question at two scales,
            // and having them here while the window history lived elsewhere
            // split one subject across two lenses. Overview keeps usage.
            ForEach(OverviewCard.visible(hiddenRaw: hiddenCardsRaw), id: \.self) { card in
                self.card(card)
            }
        }
    }

    /// One card, or nothing when it does not apply to this tab. A card absent
    /// because the surface has no use for it is not the same as one the user
    /// hid, and neither needs a placeholder.
    @ViewBuilder
    private func card(_ card: OverviewCard) -> some View {
        switch card {
        case .quotaSummary:
            // `limitsEnabled` too, not just the tab. Settings promises "Off
            // hides the Agent-limits quota card everywhere — the Overview
            // summary, every client's own tab, and this preview", and this is
            // that Overview summary; only the `.limits` branch below was
            // honouring it.
            if singleClient == nil, limitsEnabled {
                let todayFigure = stats.perDayMap[Format.todayKey()].map {
                    (tokens: $0.tokens, cost: $0.cost)
                }
                QuotaSummaryLine(
                    summary: quotaSummary, attempted: usageAttempted, today: todayFigure)
                    // With other devices selected, the quota and "Today" are
                    // still this Mac's: say so in the card's corners. "Today"
                    // is the card's last row, so its note sits bottom right.
                    .overlay(alignment: .topTrailing) {
                        if merged != nil { thisMacNote(DeviceOverlayCopy.quotaThisMacOnly) }
                    }
                    .overlay(alignment: .bottomTrailing) {
                        if merged != nil, todayFigure != nil, quotaSummary != nil {
                            thisMacNote(DeviceOverlayCopy.thisMacOnly)
                        }
                    }
            }
        case .limits:
            // Back on Overview as of 2026-08-17. Moving it to the Quota lens
            // took the most-looked-at card off the landing tab for everyone
            // rather than for anyone who asked; it belongs in both, as a glance
            // here and in full there. Hiding it is now the user's call.
            if limitsEnabled {
                AgentLimitsCard(
                    clients: clientIds, trace: trace, agentUsage: agentUsage,
                    usageAttempted: usageAttempted,
                    title: singleClient.map {
                        "%@ limits".localized(ClientRegistry.tabDisplayName($0))
                    } ?? "Agent limits",
                    note: singleClient == nil
                        ? "OAuth quota" : "Session / weekly / model limits",
                    restrict: singleClient != nil,
                    reorderable: singleClient == nil,
                    // Both tabs. This gate was a copy of the one in
                    // `PopoverView`, and removing that one left this one
                    // blanking the same indicator on the same lens — the rule
                    // was stated twice and reconciled once. The client tab's
                    // card carries the recent-consumption indicator too, and it
                    // is computed from these curves.
                    curves: windowCurves)
            }
        case .chart:
            if let merged {
                combined(merged) {
                    if let graph = merged.graph {
                        UsageChartCard(
                            payload: graph.payload, clientIds: merged.clientIds,
                            stats: graph.stats, colors: merged.colors, year: year)
                    } else {
                        loadingCard("Token Usage", failed: merged.graphFailed)
                    }
                }
            } else if singleClient != nil && !hasLocalUsage {
                DashCard("Token Usage") {
                    Text("No local usage records in this range.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            } else {
                chart
            }
        case .trace:
            if singleClient == nil {
                UsageTraceCard(buckets: trace, windowSecs: 600, hidden: hidden)
            }
        case .models:
            let title = singleClient.map {
                "%@ models".localized(ClientRegistry.style($0).displayName)
            } ?? "Models by cost"
            if let merged {
                combined(merged) {
                    if merged.models != nil || merged.modelsFailed {
                        ModelBreakdownCard(
                            report: merged.models, clientIds: merged.modelClientIds,
                            colors: merged.colors, title: title, loading: false)
                    } else {
                        loadingCard(title, failed: false)
                    }
                }
            } else {
                ModelBreakdownCard(
                    report: modelReport, clientIds: clientIds, colors: colors,
                    title: title, loading: modelLoading)
            }
        case .streaks:
            if let merged {
                combined(merged) {
                    if let graph = merged.graph {
                        StreaksCard(streaks: graph.stats.streaks)
                    } else {
                        loadingCard("Streaks", failed: merged.graphFailed)
                    }
                }
            } else {
                StreaksCard(streaks: stats.streaks)
            }
        }
    }

    private func thisMacNote(_ text: String) -> some View {
        Text(text.localized)
            .font(.caption2)
            .foregroundStyle(.tertiary)
            .padding(Self.noteInset)
    }

    /// The notes sit in the card's padding, clear of its text.
    private static let noteInset: CGFloat = 6

    /// A combined card with its "N devices combined" label above it.
    private func combined<Content: View>(
        _ merged: OverviewMerge, @ViewBuilder _ card: () -> Content
    ) -> some View {
        VStack(spacing: Self.badgeGap) {
            DevicesCombinedBadge(count: merged.deviceCount)
            card()
        }
    }

    /// Gap between the label and its card: closer to the card than to the one above.
    private static let badgeGap: CGFloat = 4

    private func loadingCard(_ title: String, failed: Bool) -> some View {
        DashCard(title) {
            if failed {
                Text(DeviceOverlayCopy.failedNoData.localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                LoadingLine(title: DeviceOverlayCopy.loading)
            }
        }
    }

    private var chart: some View {
        UsageChartCard(
            payload: payload, clientIds: clientIds, stats: stats, colors: colors,
            year: year)
    }
}
