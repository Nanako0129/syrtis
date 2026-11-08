import SwiftUI
import TokenBarCore

/// "Models by cost" lens, port of ModelsView.tsx (itself adapted from
/// tokscale's TUI models view). One row per model sorted by cost:
/// provider-tinted dot + name + share of total cost, a dim In·Out·CR·CW token
/// split, and the model's totals with cost in green. No row cap — it scrolls.
struct ModelsView: View {
    let report: ModelReport?
    /// Restrict rows to these clients; empty = show everything.
    var clientIds: [String] = []
    let colors: ModelColorMap
    /// See `ModelBreakdownCard.loading`: an absent report during the deferred
    /// fetch is not the same as a completed fetch that found nothing.
    var loading = false

    @State private var hover: HoverState?
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

    private struct HoverState {
        let entry: ModelReportEntry
        /// Cursor location in the rows container's coordinate space.
        let point: CGPoint
    }

    private static let rowsSpace = "models-rows"

    private static let kinds: [(label: String, pick: (ModelReportEntry) -> Int64)] = [
        ("In", { $0.input }),
        ("Out", { $0.output }),
        ("CR", { $0.cacheRead }),
        ("CW", { $0.cacheWrite }),
    ]

    var body: some View {
        let rows = (report?.tabEntries(clientIds: clientIds) ?? [])
            .sorted { $0.cost != $1.cost ? $0.cost > $1.cost : $0.total > $1.total }
        let totalCost = rows.reduce(0) { $0 + $1.cost }
        let totalTokens = rows.reduce(Int64(0)) { $0.saturatingAdding($1.total) }

        DashCard(
            "Models by cost",
            trailing: {
                VStack(alignment: .trailing, spacing: 1) {
                    // Whole sentence per key, so a translation can add a
                    // measure word and reorder; only the plural form branches.
                    Text((rows.count == 1 ? "%lld model · %@ · %@" : "%lld models · %@ · %@")
                        .localized(
                            rows.count, Format.compactTokens(totalTokens),
                            Format.money(tokens: totalTokens, cost: totalCost)))
                        .foregroundStyle(.secondary)
                    if let updatedAt = report?.pricingUpdatedAt {
                        Text("Prices updated %@".localized(Format.relativeTime(updatedAt)))
                            .foregroundStyle(.tertiary)
                            .help("LiteLLM pricing data; refreshes automatically about once an hour")
                    }
                }
                .font(.caption2)
            }
        ) {
            if rows.isEmpty, report == nil, loading {
                ProgressView()
                    .controlSize(.small)
                    .accessibilityLabel("Loading…")
            } else if rows.isEmpty {
                Text("No model usage in this range")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 10) {
                    ForEach(rows, id: \.rowID) { entry in
                        row(entry, totalCost: totalCost)
                    }
                }
                .coordinateSpace(name: Self.rowsSpace)
                .overlay(alignment: .topLeading) {
                    if let hover {
                        GeometryReader { geo in
                            let measuredSize = tooltipSize == .zero
                                ? CGSize(width: ModelUsageTooltip.width, height: 120)
                                : tooltipSize
                            let offset = PopoverTooltipPlacement.offset(
                                anchor: hover.point,
                                tooltipSize: measuredSize,
                                containerFrame: geo.frame(in: .global),
                                viewport: popoverScrollViewport)
                            tooltip(hover.entry)
                                .offset(offset ?? .zero)
                        }
                        // GeometryReader fills the rows; keep hits on the rows
                        // so continuous hover does not end while the tooltip
                        // is up.
                        .allowsHitTesting(false)
                    }
                }
            }
        }
    }

    private func row(_ entry: ModelReportEntry, totalCost: Double) -> some View {
        let share = totalCost > 0 ? entry.cost / totalCost * 100 : 0
        let isHovered = hover?.entry.rowID == entry.rowID
        return HStack(spacing: 8) {
            Circle()
                .fill(Color(hex: colors.color(entry.provider, entry.model)))
                .frame(width: 8, height: 8)
                .overlay {
                    Circle().stroke(
                        Color.primary.opacity(isHovered ? 0.85 : 0),
                        lineWidth: 1)
                }
                .shadow(
                    color: Color.primary.opacity(isHovered ? 0.65 : 0),
                    radius: isHovered ? 3 : 0)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(entry.model)
                        .font(.caption)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Text(String(format: "%.1f%%", share))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.tertiary)
                }
                HStack(spacing: 8) {
                    ForEach(Self.kinds, id: \.label) { kind in
                        (Text(kind.label.localized + " ").foregroundStyle(.tertiary)
                            + Text(Format.compactTokens(kind.pick(entry))))
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 2) {
                Text(Format.compactTokens(entry.total))
                    .font(.caption.monospacedDigit())
                HStack(spacing: 3) {
                    if let ratio = entry.implausibleCostRatio {
                        Image(systemName: CostPlausibility.symbol)
                            .foregroundStyle(Color(hex: CostPlausibility.warningColor))
                            // The explanation otherwise lives only in the
                            // hover tooltip, which a pointer is the only way
                            // to summon — so without this the icon is the
                            // whole message and it says nothing.
                            .accessibilityLabel(
                                CostPlausibility.warningText(ratio))
                    }
                    Text(Format.money(tokens: entry.total, cost: entry.cost))
                        .foregroundStyle(Color(hex: "#22c55e"))
                }
                .font(.caption2.monospacedDigit())
            }
        }
        // Whole-row hit area and a hand-drawn tooltip, matching
        // ModelBreakdownCard/DailyView. The old `.help` only covered the model
        // name's glyph rect and carried AppKit's tooltip delay.
        .contentShape(Rectangle())
        .onContinuousHover(coordinateSpace: .named(Self.rowsSpace)) { phase in
            switch phase {
            case let .active(point):
                hover = HoverState(entry: entry, point: point)
            case .ended:
                // Guard on identity: this list scrolls, so an `.ended` from a
                // row the cursor already left must not clear the current one.
                if hover?.entry.rowID == entry.rowID {
                    hover = nil
                }
            }
        }
    }

    // MARK: - Hover tooltip

    private func tooltip(_ entry: ModelReportEntry) -> some View {
        ModelUsageTooltip(
            model: entry.model,
            provider: entry.provider,
            // A row merged across the tab's clients names them in its parts.
            context: entry.clientParts == nil ? ClientRegistry.style(entry.client).displayName : nil,
            color: colors.color(entry.provider, entry.model),
            input: entry.input,
            output: entry.output,
            cacheRead: entry.cacheRead,
            cacheWrite: entry.cacheWrite,
            reasoning: entry.reasoning,
            total: entry.total,
            cost: entry.cost,
            costRatio: entry.implausibleCostRatio,
            clientParts: entry.clientParts,
            measuredSize: $tooltipSize)
    }
}
