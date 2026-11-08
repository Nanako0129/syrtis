import SwiftUI
import TokenBarCore

enum ModelBarGeometry {
    static let gap: CGFloat = 1

    static func widths(values: [Int64], totalWidth: CGFloat) -> [CGFloat] {
        guard !values.isEmpty, totalWidth > 0 else { return [] }
        let available = max(0, totalWidth - gap * CGFloat(values.count - 1))
        let minimum = min(1, available / CGFloat(values.count))
        let remainder = available - minimum * CGFloat(values.count)
        let weights = values.map { Double(max(0, $0)).squareRoot() }
        let totalWeight = weights.reduce(0, +)
        guard totalWeight > 0 else { return Array(repeating: 0, count: values.count) }
        return weights.map { minimum + remainder * CGFloat($0 / totalWeight) }
    }
}

/// "Models" card: one row per model sorted by cost, with the token-category
/// split as a stacked bar. Port of ModelBreakdownCard.tsx — bar widths use a
/// square-root scale (cache reads dwarf everything linearly; log flattens
/// everything), while the hover help reports true counts and linear shares.
struct ModelBreakdownCard: View {
    let report: ModelReport?
    /// Restrict rows to these clients; empty = show everything.
    var clientIds: [String] = []
    let colors: ModelColorMap
    var title = "Models"
    /// The model report is fetched after the graph commits, so an absent report
    /// means "still loading" until that request finishes. Rendering the empty
    /// copy in that window would tell the user they have no model usage.
    var loading = false

    @State private var expanded = false
    @State private var hover: HoverState?
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

    private struct HoverState {
        let entry: ModelReportEntry
        /// Cursor location in the rows container's coordinate space.
        let point: CGPoint
    }

    private static let maxRows = 8
    private static let rowsSpace = "model-rows"

    /// Token-category palette (matches the Tauri .model-seg-* CSS classes).
    private static let picks: [(ModelReportEntry) -> Int64] = [
        { $0.input }, { $0.output }, { $0.cacheRead }, { $0.cacheWrite }, { $0.reasoning },
    ]
    private static let tokenKinds: [(label: String, color: String, pick: (ModelReportEntry) -> Int64)] =
        zip(TokenKindPalette.all, picks).map { ($0.label, $0.color, $1) }

    var body: some View {
        let rows = (report?.tabEntries(clientIds: clientIds) ?? [])
            .sorted { $0.cost != $1.cost ? $0.cost > $1.cost : $0.total > $1.total }
        let totalCost = rows.reduce(0) { $0 + $1.cost }

        DashCard(
            title,
            trailing: {
                VStack(alignment: .trailing, spacing: 1) {
                    Text((rows.count == 1 ? "%lld model · %@" : "%lld models · %@")
                        .localized(rows.count, Format.usd(totalCost)))
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
                legend
                VStack(spacing: 8) {
                    ForEach(
                        expanded ? rows : Array(rows.prefix(Self.maxRows)),
                        id: \.self.rowID
                    ) { entry in
                        row(entry)
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
                let hidden = rows.count - min(rows.count, Self.maxRows)
                if hidden > 0 {
                    let title = expanded
                        ? "Show less".localized
                        : "Show %lld more".localized(hidden)
                    Button(title) {
                        expanded.toggle()
                    }
                    .buttonStyle(.plain)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(.secondary)
                }
            }
        }
        .zIndex(hover == nil ? 0 : 1)
    }

    private var legend: some View {
        FlowLayout(hSpacing: 8, vSpacing: 3) {
            ForEach(Self.tokenKinds, id: \.label) { kind in
                HStack(spacing: 4) {
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Color(hex: kind.color))
                        .frame(width: 7, height: 7)
                    Text(kind.label.localized)
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
        }
    }

    private func row(_ entry: ModelReportEntry) -> some View {
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
            VStack(alignment: .leading, spacing: 3) {
                Text(entry.model)
                    .font(.caption)
                    .lineLimit(1)
                    .truncationMode(.middle)
                segmentBar(entry, isHovered: isHovered)
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 2) {
                Text(Format.compactTokens(entry.total))
                    .font(.caption.monospacedDigit())
                HStack(spacing: 3) {
                    if let ratio = entry.implausibleCostRatio {
                        Image(systemName: CostPlausibility.symbol)
                            .foregroundStyle(Color(hex: CostPlausibility.warningColor))
                            // See ModelsView: the tooltip carrying this
                            // explanation is pointer-only, so the icon needs
                            // to say it itself.
                            .accessibilityLabel(
                                CostPlausibility.warningText(ratio))
                    }
                    Text(Format.money(tokens: entry.total, cost: entry.cost))
                        .foregroundStyle(Color(hex: "#22c55e"))
                }
                .font(.caption2.monospacedDigit())
            }
        }
        // Float the rich tooltip near the cursor anywhere on the row — the
        // web card tracks just the bar, but a 6pt-tall hover target is too
        // fiddly with a real pointer.
        .contentShape(Rectangle())
        .onContinuousHover(coordinateSpace: .named(Self.rowsSpace)) { phase in
            switch phase {
            case let .active(point):
                hover = HoverState(entry: entry, point: point)
            case .ended:
                hover = nil
            }
        }
    }

    /// sqrt-scaled stacked category bar.
    private func segmentBar(_ entry: ModelReportEntry, isHovered: Bool) -> some View {
        let segments = Self.tokenKinds
            .map { (color: $0.color, value: $0.pick(entry)) }
            .filter { $0.value > 0 }
        return GeometryReader { geo in
            let widths = ModelBarGeometry.widths(
                values: segments.map(\.value), totalWidth: geo.size.width)
            HStack(spacing: ModelBarGeometry.gap) {
                ForEach(segments.indices, id: \.self) { i in
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Color(hex: segments[i].color))
                        .frame(width: widths[i])
                }
            }
        }
        .frame(height: 6)
        .overlay {
            RoundedRectangle(cornerRadius: 1.5)
                .stroke(Color.primary.opacity(isHovered ? 0.85 : 0), lineWidth: 1)
        }
        .shadow(
            color: Color.primary.opacity(isHovered ? 0.65 : 0),
            radius: isHovered ? 3 : 0)
    }

    // MARK: - Hover tooltip

    /// True token counts and linear shares per category — the bar itself is
    /// sqrt-scaled, so this is where the real numbers live.
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

struct ModelUsageTooltip: View {
    static let width: CGFloat = 210

    let model: String
    let provider: String
    let context: String?
    let color: String
    let input: Int64
    let output: Int64
    let cacheRead: Int64
    let cacheWrite: Int64
    let reasoning: Int64
    let total: Int64
    let cost: Double
    /// See `ModelReportEntry.implausibleCostRatio`. Defaults to nil so the day/month
    /// slice callers, which have no pricing comparison, need not pass it.
    var costRatio: Double? = nil
    /// Per-client token totals of a row merged across a tab's clients
    /// (`ModelReport.tabEntries`); nil for a single-client row.
    var clientParts: [ClientPart]? = nil
    @Binding var measuredSize: CGSize

    private var kinds: [(label: String, color: String, value: Int64)] {
        zip(TokenKindPalette.all, [input, output, cacheRead, cacheWrite, reasoning])
            .map { (label: $0.label, color: $0.color, value: $1) }
            .filter { $0.value > 0 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .top, spacing: 5) {
                Circle()
                    .fill(Color(hex: color))
                    .frame(width: 6, height: 6)
                    // Keeps the dot on the first line's optical centre once
                    // the name below wraps to a second one.
                    .padding(.top, 3)
                // Wraps instead of truncating: the rows themselves have to
                // middle-truncate (a fixed-width column), so this is the only
                // place a long identifier can be read in full.
                //
                // Measured, not estimated — caption1 is 10pt and the text has
                // 183pt after the panel's padding, the dot and the spacing,
                // giving a 12pt line height:
                //
                //     deepseek/deepseek-v4-flash-vision-exp   37 chars, 2 lines
                //     nousresearch/hermes-3-llama-3.1-405b-…  45 chars, 2 lines
                //     4-line capacity                         76 chars worst
                //                                             case (all 'm'),
                //                                             102 for a mixed
                //                                             model-id alphabet
                //
                // The longest identifiers in circulation are around 45
                // characters, so four lines clears them by more than half
                // again. The cap stays because middle truncation is the last
                // resort against corrupt input growing the panel without
                // bound; `lineLimit` costs nothing on the ordinary one- and
                // two-line names.
                Text(model)
                    .font(.caption.weight(.semibold))
                    .lineLimit(4)
                    .truncationMode(.middle)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Text([context, provider].compactMap { $0 }.joined(separator: " · "))
                .font(.caption2)
                .foregroundStyle(.tertiary)
            if let clientParts {
                ForEach(clientParts, id: \.client) { part in
                    HStack {
                        Text(ClientRegistry.style(part.client).displayName)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        Spacer()
                        Text("\(Format.compactTokens(part.total)) · \(Int((Double(part.total) / Double(max(1, total)) * 100).rounded()))%")
                            .foregroundStyle(.secondary)
                    }
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                }
            }
            HStack {
                Text("%@ tokens".localized(Format.compactTokens(total)))
                Spacer()
                Text(Format.money(tokens: total, cost: cost))
            }
            .font(.caption2)
            .foregroundStyle(.secondary)
            if let costRatio {
                // Named in multiples rather than "wrong": the app cannot know
                // the client's real rate, only that this is far off any price
                // the local table can justify.
                Text(CostPlausibility.warningText(costRatio))
                    .font(.caption2)
                    .foregroundStyle(Color(hex: CostPlausibility.warningColor))
                    .fixedSize(horizontal: false, vertical: true)
            }
            ForEach(kinds, id: \.label) { kind in
                HStack(spacing: 4) {
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Color(hex: kind.color))
                        .frame(width: 6, height: 6)
                    Text(kind.label.localized)
                    Spacer()
                    Text("\(Format.compactTokens(kind.value)) · \(Int((Double(kind.value) / Double(max(1, total)) * 100).rounded()))%")
                        .foregroundStyle(.secondary)
                }
                .font(.caption2)
            }
        }
        .padding(8)
        .frame(width: Self.width, alignment: .leading)
        .tooltipSurface()
        .onGeometryChange(for: CGSize.self) { $0.size } action: { measuredSize = $0 }
        .allowsHitTesting(false)
    }
}

extension ModelReportEntry {
    /// Stable row identity across re-sorts (client+model+provider triple). A
    /// `tabKeyed` row is unique by model+provider in its tab and its client
    /// depends on the engine's row order, so the client is left out; a flagged
    /// row in such a tab keeps the triple and cannot collide with it.
    var rowID: String {
        tabKeyed ? "*|\(model)|\(provider)" : "\(client)|\(model)|\(provider)"
    }
}

extension CostPlausibility {
    /// The one wording for a flagged cost, used by the tooltip line and by the
    /// icon's accessibility label. Both have to say the same thing, and the
    /// icon is the only channel a VoiceOver or keyboard user has — the tooltip
    /// that carries this text is summoned by a pointer and nothing else.
    ///
    /// Lives here rather than beside the threshold in TokenBarCore because it
    /// needs `Format` and `.localized`, which are Syrtis-side.
    static func warningText(_ ratio: Double) -> String {
        "Cost reported by the client, about %@x the local price estimate"
            .localized(Format.compactRatio(ratio))
    }
}

