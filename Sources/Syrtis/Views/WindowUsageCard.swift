import SwiftUI
import TokenBarCore

/// One quota window: the line is the subscription's quota, the bars underneath
/// are the local usage that moved it. All geometry comes from
/// `WindowCardGeometry`, so this file only strokes and fills.
///
/// Why the bars sit behind the line: in "used" mode early in a window the line
/// runs low, straight through the bar band. Rather than move either series, the
/// bars recede and light one at a time under the cursor while the line stays in
/// front (`V-5`).
struct WindowUsageCard: View {
    let state: WindowCardState
    /// Nil on a client with no live account, which draws exactly as before.
    var account: CardAccountContext?
    var onSelectAccount: (String?) -> Void = { _ in }

    /// Set alongside the geometry so the tooltip can slice the same messages
    /// the bars were built from.
    private var hoverMessages: [WindowMessage] { state.usageHalf?.mine ?? [] }
    private var hoverSubtitle: String {
        guard let q = state.quotaHalf else { return "" }
        return "\(ClientRegistry.style(q.clientId).displayName) · \(q.windowLabel.localized)"
    }

    /// The window card's title and window pills are narrow; Antigravity's
    /// grouped buckets arrive as "Gemini Models · Weekly Limit Remaining",
    /// which overflowed the pill row (maintainer, 2026-10-03). Shortens only
    /// the "<group> · <bucket>" shape: "Gemini · Weekly", "Claude/GPT · 5h"
    /// (localized). A bucket that repeats its group's name, "Gemini 4 Argon ·
    /// Gemini 4 Argon Weekly Limit" (measured 2026-11-04), shortens the same
    /// way: "Gemini 4 Argon · Weekly". Any other label is returned localized
    /// and unchanged, and the Agent-limits card keeps the full name.
    static func shortLabel(_ label: String) -> String {
        let parts = label.components(separatedBy: " · ")
        guard parts.count == 2 else { return label.localized }
        let bucket: String
        if parts[1].hasSuffix(" Limit Remaining") {
            bucket = String(parts[1].dropLast(" Limit Remaining".count))
        } else if parts[1].hasPrefix(parts[0] + " "), parts[1].hasSuffix(" Limit"),
                  parts[1].count > parts[0].count + 1 + " Limit".count {
            bucket = String(parts[1].dropFirst(parts[0].count + 1).dropLast(" Limit".count))
        } else {
            return label.localized
        }
        var group = parts[0]
        for suffix in [" Models", " models"] where group.hasSuffix(suffix) {
            group = String(group.dropLast(suffix.count))
        }
        group = group.replacingOccurrences(of: " and ", with: "/")
        let window = switch bucket {
        case "Weekly": "Weekly".localized
        case "Five Hour": "5h".localized
        default: bucket.localized
        }
        return "\(group) · \(window)"
    }

    @AppStorage("tokenbar.limits.asUsed") private var asUsed = false
    @AppStorage(WindowCardLoader.selectionKey) private var selection = ""
    /// The whole card's frame, not the chart's. The tooltip is clamped to this
    /// so it stays inside the card: escaping it means the next card's glass
    /// chrome and content paint over the tooltip, which is what a "border in
    /// the foreground" actually is.
    @State private var cardFrame: CGRect = .zero
    /// The hovered zone and its anchor, both kept so the tooltip can live at
    /// card level. Third attempt at this: the tooltip must be laid out in the
    /// same coordinate space it is clamped to, or the offset the placement
    /// helper returns is measured from one origin and applied to another.
    @State private var hoverZone: HitZone?
    @State private var hoverAnchorInCard: CGPoint = .zero
    @State private var hover: Int?
    @State private var hoverPoint: CGPoint = .zero
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var viewport

    /// Stage-1 placeholders and stage-2 content read the SAME constants, so
    /// the card cannot change height when the scan lands. That is structural:
    /// there is no second number to keep in step.
    private static let chartHeight: CGFloat = 96
    private static let legendHeight: CGFloat = 13
    private static let footerHeight: CGFloat = 15
    /// The bar band is a third of the height. The line uses the whole box, so
    /// the two overlap by design — see the type comment.
    private static let barBand: CGFloat = 32
    private static let tooltipWidth: CGFloat = 190

    private var metric: QuotaMetric { asUsed ? .used : .remaining }


    var body: some View {
        switch state {
        case .loading:
            DashCard("Session window", subtitle: "Loading") {
                placeholder("Waiting for quota…")
            }
        case let .blocked(clientId, reason):
            // Not "Waiting for quota": this state is now also reached once the
            // fetch has settled without an answer, and a card whose body says
            // it could not load while its subtitle says it is still waiting
            // contradicts itself.
            DashCard("%@ window".localized(ClientRegistry.style(clientId).displayName),
                     subtitle: "Quota unavailable") {
                Text(reason)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        case let .noQuotaHistory(_, label, candidates, cardId):
            DashCard("%@ window".localized(Self.shortLabel(label)), subtitle: "No quota history",
                     titleAccessory: accountHeaderLabel) {
                accountPills
                windowButtons(candidates: candidates, cardId: cardId)
                Text("This window has no recorded quota history, so there is no line to draw.".localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        case let .quotaOnly(quota, scanFailed):
            card(quota, usage: nil, scanFailed: scanFailed)
                .panelSwitchAnimation("\(asUsed)|\(selection)")
        case let .ready(quota, usage):
            card(quota, usage: usage, scanFailed: false)
                .panelSwitchAnimation("\(asUsed)|\(selection)")
        }
    }

    /// One layout for both stages. `usage == nil` swaps content for
    /// placeholders of the same declared height; nothing else differs.
    @ViewBuilder
    private func card(
        _ quota: WindowQuotaHalf, usage: WindowUsageHalf?, scanFailed: Bool
    ) -> some View {
        // Header label of a card showing a non-primary account, so it cannot be
        // read as the primary's (spec 1b): directly after the title, the
        // placement both platforms agreed (2026-10-03); absent for the primary.
        DashCard("%@ window".localized(Self.shortLabel(quota.windowLabel)), subtitle: stateLine(quota),
                 titleAccessory: accountHeaderLabel) {
            SegmentedPicker(
                selection: Binding(get: { asUsed }, set: { asUsed = $0 }),
                options: [(value: false, label: "Remaining"), (value: true, label: "Used")])
        } content: {
            accountPills
            windowButtons(candidates: quota.candidates, cardId: quota.cardId)
            if quota.placementPending {
                placeholder("Placing the window…")
            } else if let interval = WindowCardLoader.interval(quota.resolution) {
                let q = WindowCardGeometry.quotaGeometry(
                    windowStartMs: interval.start, windowEndMs: interval.end,
                    nowMs: min(quota.nowMs, interval.end),
                    samples: quota.samples, metric: metric)
                let geo = ChartGeometry(
                    nowX: q.nowX, firstSampleX: q.firstSampleX,
                    bars: usage?.bars ?? [], hits: usage?.hits ?? [],
                    samplePoints: q.samplePoints, curve: q.curve)
                headline(geo)
                chart(geo, window: quota.cardId).zIndex(1)
                legend(geo).frame(height: Self.legendHeight)
                undatedNote(usage)
                scopeNote(usage, label: quota.windowLabel)
                Group {
                    switch CardAccountContext.localUsageSlot(
                        account, hasUsage: usage != nil, scanFailed: scanFailed) {
                    case .notAttributable:
                        // Rule 6: this account's usage cannot be scoped from
                        // local logs, or its scan failed. One fixed line, in
                        // place of numbers that would be another account's.
                        Text("Local usage can't be attributed to this account yet.".localized)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    case .numbers:
                        if let usage {
                            equivalenceRow(
                                quota: quota, mine: usage.mine, interval: interval)
                        }
                    case .unreadable:
                        // A settled failure, not a slow success. The chart above
                        // is drawn from the quota half and stays correct; only
                        // this line has nothing to report.
                        Text("Local usage could not be read.".localized)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    case .spinner:
                        LoadingLine(title: "Reading local usage…")
                    }
                }
                .frame(height: Self.footerHeight, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                Text(emptyText(quota))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .overlay(alignment: .topLeading) { tooltipLayer }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: {
            cardFrame = $0
        }
        .zIndex(hover == nil ? 0 : 1)
    }

    /// The chart's slot, held open at its real height so the card does not
    /// grow when content arrives.
    private func placeholder(_ title: String) -> some View {
        LoadingLine(title: title)
            .frame(height: Self.chartHeight + Self.legendHeight + Self.footerHeight,
                   alignment: .topLeading)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var accountHeaderLabel: AnyView? {
        AccountPills.headerLabel(account).map { label in
            AnyView(Text(verbatim: label)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .help(account?.tooltip ?? ""))
        }
    }

    @ViewBuilder
    private var accountPills: some View {
        if let account {
            AccountPills(
                clientId: account.clientId, accounts: account.accounts,
                primaryEmail: account.primaryEmail,
                selected: account.resolved, select: onSelectAccount)
        }
    }

    @ViewBuilder
    private func windowButtons(
        candidates: [(cardId: String, label: String)], cardId: String
    ) -> some View {
        if candidates.count > 1 {
            SegmentedPicker(
                // Present-in-candidates, not merely non-empty. The stored
                // selection is one preference shared by every client, so it
                // routinely names another client's window — and a provider can
                // stop offering one. `WindowCardLoader.select` already falls
                // back to `cardId` in both cases; echoing the stored value here
                // left the picker with a selection matching none of its
                // options, so the card drew one window while no button was
                // highlighted.
                selection: Binding(
                    get: {
                        candidates.contains { $0.cardId == selection }
                            ? selection : cardId
                    },
                    set: { selection = $0 }),
                options: candidates.map { (value: $0.cardId, label: Self.shortLabel($0.label)) },
                wraps: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    // MARK: - Header

    private func stateLine(_ quota: WindowQuotaHalf) -> String {
        if quota.placementPending { return "Placing the window" }
        switch quota.resolution {
        case let .active(_, end):
            return "Resets in %@".localized(Format.duration(ms: end - quota.nowMs))
        case let .inferred(_, end):
            return "Inferred window · resets in %@".localized(
                Format.duration(ms: end - quota.nowMs))
        case .idle: return "No window running"
        case .unavailable: return "Window unavailable"
        }
    }

    private func emptyText(_ quota: WindowQuotaHalf) -> String {
        switch quota.resolution {
        case .idle:
            return "The last window ended and nothing has been used since — no window is running.".localized
        case .unavailable:
            return "This subscription did not report a usable reset time, so the window cannot be placed.".localized
        default: return ""
        }
    }

    @ViewBuilder
    private func headline(_ geo: ChartGeometry) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let latest = geo.samplePoints.last {
                Text(verbatim: "\(Int(latest.y.rounded()))%")
                    .font(.system(size: 22, weight: .semibold))
                Text((asUsed ? "used" : "remaining").localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("No quota reading in this window".localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - Chart

    private func chart(_ geo: ChartGeometry, window: String) -> some View {
        GeometryReader { proxy in
            let w = proxy.size.width
            let h = Self.chartHeight
            ZStack(alignment: .topLeading) {
                QuotaChartStage(geo: geo, window: window, asUsed: asUsed) {
                    draw($0, geo: $1, w: w, h: h)
                }
                .frame(width: w, height: h)
                if let hover, geo.hits.indices.contains(hover) {
                    overlay(geo, index: hover, w: w, h: h)
                }
            }
            .contentShape(Rectangle())
            .onContinuousHover { phase in
                switch phase {
                case let .active(p):
                    hoverPoint = p
                    // Zones tile [0, nowX] exactly, so this both finds the zone
                    // and rejects the future in one step: past nowX nothing
                    // contains the point.
                    let x = p.x / max(w, 1)
                    let index = geo.hits.firstIndex { x >= $0.x && x < $0.x + $0.width }
                    let zone = index.map { geo.hits[$0] }
                    hover = index
                    hoverZone = zone
                    let chart = proxy.frame(in: .global)
                    // Anchor X pins to the zone edge, Y follows the cursor, and
                    // both are translated into the card's space here — the one
                    // place that knows both frames. Read the local `zone`, not
                    // the @State we just wrote: within one closure run that
                    // still holds the previous value, and the anchor would lag
                    // a zone behind the cursor.
                    let anchorX: CGFloat = zone.map {
                        CGFloat($0.x + $0.width) * w
                    } ?? p.x
                    hoverAnchorInCard = CGPoint(
                        x: anchorX + chart.minX - cardFrame.minX,
                        y: p.y + chart.minY - cardFrame.minY)
                case .ended:
                    hover = nil
                    hoverZone = nil
                }
            }
        }
        .frame(height: Self.chartHeight)
    }

    private func draw(_ ctx: GraphicsContext, geo: ChartGeometry, w: CGFloat, h: CGFloat) {
        // Hatch means one thing: no quota sample here, so no line. Both the
        // pre-sampling stretch and the future qualify, so both get one weight;
        // whether usage data exists is answered by the bars drawn over it.
        for (from, to) in [(0.0, geo.firstSampleX), (geo.nowX, 1.0)] where to > from {
            let rect = CGRect(x: from * w, y: 0, width: (to - from) * w, height: h)
            ctx.fill(Path(rect), with: .color(.secondary.opacity(0.06)))
            var stripes = Path()
            var x = rect.minX - h
            while x < rect.maxX {
                stripes.move(to: CGPoint(x: x, y: h))
                stripes.addLine(to: CGPoint(x: x + h, y: 0))
                x += 5
            }
            // Scoped so the clip cannot leak onto the bars and line below.
            ctx.drawLayer { layer in
                layer.clip(to: Path(rect))
                layer.stroke(stripes, with: .color(.secondary.opacity(0.18)), lineWidth: 0.6)
            }
        }

        let usage = state.usageHalf
        for (i, bar) in geo.bars.enumerated() {
            // Past the scan: no bar and no baseline tick, because the tick says
            // "nothing was spent" and nothing has been read here yet.
            if let usage, usage.isPending(geo.hits[i]) { continue }
            let x = bar.x * w
            let bw = max(bar.width * w - 1, 0.5)
            if bar.isEmpty {
                ctx.fill(Path(CGRect(x: x, y: h - 1, width: bw, height: 1)),
                         with: .color(.primary.opacity(0.28)))
                continue
            }
            let bh = max(bar.height * Self.barBand, 1)
            ctx.fill(Path(CGRect(x: x, y: h - bh, width: bw, height: bh)),
                     with: .color(.accentColor.opacity(hover == i ? 0.62 : 0.17)))
        }

        // Scoped to the LINE. This was a `guard … else { return }`, which also
        // skipped the sample dots thirteen lines below — so a window with one
        // reading drew nothing at all while the caption beside it said "1
        // reading". `monotoneCurve` needs two points to interpolate between;
        // that is a fact about the line, and it was applied to the dots.
        if geo.curve.count > 1 {
            var path = Path()
            for (i, p) in geo.curve.enumerated() {
                let pt = CGPoint(x: p.x * w, y: y(p.y, h: h))
                i == 0 ? path.move(to: pt) : path.addLine(to: pt)
            }
            // A ground-coloured underlay keeps the line legible where it crosses
            // a lit bar, so "the line is always in front" holds by construction.
            ctx.stroke(path, with: .color(Color(nsColor: .windowBackgroundColor).opacity(0.85)),
                       style: StrokeStyle(lineWidth: 3.4, lineCap: .round, lineJoin: .round))
            ctx.stroke(path, with: .color(.accentColor),
                       style: StrokeStyle(lineWidth: 1.8, lineCap: .round, lineJoin: .round))
        }

        for p in geo.samplePoints {
            let r = CGRect(x: p.x * w - 2, y: y(p.y, h: h) - 2, width: 4, height: 4)
            ctx.fill(Path(ellipseIn: r), with: .color(Color(nsColor: .windowBackgroundColor)))
            ctx.stroke(Path(ellipseIn: r), with: .color(.accentColor), lineWidth: 1.1)
        }
    }

    /// Fixed 0...100: rescaling would make 7% used and 63% used look the same,
    /// which is the one thing this card exists to tell apart.
    private func y(_ value: Double, h: CGFloat) -> CGFloat {
        10 + (1 - value / 100) * (h - 16)
    }

    // MARK: - Hover

    @ViewBuilder
    private func overlay(_ geo: ChartGeometry, index: Int, w: CGFloat, h: CGFloat)
        -> some View
    {
        let zone = geo.hits[index]
        let anchorX = (zone.x + zone.width) * w
        Path { p in
            p.move(to: CGPoint(x: anchorX, y: 0))
            p.addLine(to: CGPoint(x: anchorX, y: h))
        }
        .stroke(Color.primary.opacity(0.3), lineWidth: 1)
        .allowsHitTesting(false)
        if let sample = zone.closingSample {
            Circle()
                .fill(Color.accentColor)
                .frame(width: 6, height: 6)
                .position(x: anchorX,
                          y: y(metric.value(fromUsedPercent: sample.usedPercent), h: h))
                .allowsHitTesting(false)
        }
    }

    /// Card-level, so the placement helper clamps and offsets within one
    /// coordinate space. Living inside the chart meant the offset was measured
    /// from the card's origin and applied from the chart's.
    @ViewBuilder
    private var tooltipLayer: some View {
        if let zone = hoverZone, cardFrame != .zero {
            WindowHoverTooltip(
                zone: zone, metric: metric,
                // Zone 0 owns its own lower bound, matching
                // `WindowCardGeometry.usageGeometry`. Without this the bar
                // drawn from a window-start message and the tooltip explaining
                // that bar disagree about whether the message is in it.
                messages: hoverMessages.filter {
                    let insideStart = zone.index == 0
                        ? $0.timestamp >= zone.loMs : $0.timestamp > zone.loMs
                    return insideStart && $0.timestamp <= zone.hiMs
                },
                subtitle: hoverSubtitle,
                // The scan bound comes from the live state, so "reading" clears
                // when the scan lands under a still cursor; the zone itself is
                // the one captured on hover until the cursor moves.
                pending: state.usageHalf?.isPending(zone) ?? false,
                measuredSize: $tooltipSize)
                .offset(placement(anchor: hoverAnchorInCard, container: cardFrame))
                .allowsHitTesting(false)
        }
    }

    private func placement(anchor: CGPoint, container: CGRect) -> CGSize {
        let measured = tooltipSize == .zero
            ? CGSize(width: Self.tooltipWidth, height: 120) : tooltipSize
        return PopoverTooltipPlacement.offset(
            anchor: anchor, tooltipSize: measured, containerFrame: container,
            viewport: viewport) ?? .zero
    }

    // MARK: - Footer

    private func legend(_ geo: ChartGeometry) -> some View {
        HStack(spacing: 10) {
            key(Color.accentColor, "Quota", line: true)
            key(Color.accentColor.opacity(0.5), "Usage")
            key(.secondary.opacity(0.3), "No sample")
            Spacer()
            Text("%@ readings".localized(String(geo.samplePoints.count)))
                .foregroundStyle(.tertiary)
        }
        .font(.caption2)
    }

    /// Stated, not swallowed. The engine counts rows it could not place in time
    /// precisely so a consumer cannot present a window total as definitive
    /// while omitting them; every production path dropped the count until now,
    /// leaving `--window-probe` the only thing that ever mentioned it.
    ///
    /// Worded as a fact about the SCAN, not about this card's totals. The count
    /// is scan-wide and cannot be narrowed: an undated row has no timestamp, so
    /// its window membership is not merely unavailable through the FFI, it does
    /// not exist to be recovered. Saying "not in these totals" on every card
    /// therefore claimed each card had lost a row that may belong to another
    /// subscription entirely — a per-card claim the data cannot support.
    /// The scope join found nothing. Said, not drawn as an idle week.
    ///
    /// The scope is the provider's display name and the rows carry the
    /// transcript's model id; when those do not line up the honest report is
    /// that the filter matched nothing, because empty bars under a moving curve
    /// read as "you did no work", which is a claim about the user's week that
    /// this card has no evidence for.
    @ViewBuilder
    private func scopeNote(_ usage: WindowUsageHalf?, label: String) -> some View {
        if let usage, usage.scopeMatchedNothing {
            // The label is a window identifier, so it arrives in English like
            // every other stored value — the third display site in this file
            // that has to translate it, alongside the two DashCard titles and
            // `hoverSubtitle`. Resolving it inside this helper rather than at
            // the single call site keeps the next caller from missing it.
            Text("No local usage matched %@, though this subscription has other usage in this window"
                .localized(label.localized))
                .font(.system(size: 9))
                .foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder
    private func undatedNote(_ usage: WindowUsageHalf?) -> some View {
        if let usage, usage.undatedCount > 0 {
            Text("%@ scanned rows have no usable timestamp, so no window can count them"
                .localized(String(usage.undatedCount)))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func key(_ color: Color, _ label: String, line: Bool = false) -> some View {
        HStack(spacing: 4) {
            RoundedRectangle(cornerRadius: line ? 1 : 2)
                .fill(color)
                .frame(width: line ? 12 : 8, height: line ? 2 : 8)
            Text(label.localized)
        }
        .foregroundStyle(.secondary)
    }

    private func equivalenceRow(
        quota: WindowQuotaHalf, mine: [WindowMessage],
        interval: (start: Int64, end: Int64)
    ) -> some View {
        // Restricted to the SETTLED interval, like the chart above it. Once the
        // scan refines `.idle` into `.inferred`, `quota.samples` still carries
        // readings chosen against the previous provider-anchored window, and
        // handing those to the fold let this line report the earlier cycle's
        // movement while the chart on the same card said there was no reading
        // in the window at all.
        let inWindow = quota.samples.filter {
            $0.atMs >= interval.start && $0.atMs <= interval.end
        }
        let row = WindowEquivalence.row(samples: inWindow, messages: mine)
        let plain: Bool = { if case .ratio = row { return false }; return true }()
        return Text(WindowEquivalence.text(
            row, tokens: Format.compactTokens, money: Format.usdOrBelowCent))
            .font(.caption2)
            .foregroundStyle(plain ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Same shape as the Models card's tooltip, down to the palette and the
/// zero-value filter, so one interval reads the same wherever it appears.
private struct WindowHoverTooltip: View {
    let zone: HitZone
    let metric: QuotaMetric
    let messages: [WindowMessage]
    let subtitle: String
    /// The interval reaches past the last scan: its usage is unknown, not zero.
    let pending: Bool
    @Binding var measuredSize: CGSize

    // Saturating, like every production fold over these counters: the tooltip
    // reads the same untrusted transcript rows, and a hover must not be able to
    // do what the chart underneath it no longer can.
    private var total: Int64 { messages.reduce(0) { $0.saturatingAdding($1.tokens) } }

    private var kinds: [(label: String, color: String, value: Int64)] {
        let sums: [Int64] = [
            messages.reduce(0) { $0.saturatingAdding($1.input) },
            messages.reduce(0) { $0.saturatingAdding($1.output) },
            messages.reduce(0) { $0.saturatingAdding($1.cacheRead) },
            messages.reduce(0) { $0.saturatingAdding($1.cacheWrite) },
            messages.reduce(0) { $0.saturatingAdding($1.reasoning) },
        ]
        return zip(TokenKindPalette.all, sums)
            .map { (label: $0.label, color: $0.color, value: $1) }
            .filter { $0.value > 0 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(Format.clockRange(fromMs: zone.loMs, toMs: zone.hiMs))
                .font(.caption.weight(.semibold))
            Text(subtitle)
                .font(.caption2)
                .foregroundStyle(.tertiary)
            // The quota row keeps the spike's three states; a zone with no
            // closing sample is the hatched stretch, and must say so rather
            // than show nothing.
            Text(zone.closingSample.map {
                "Quota %@%% %@".localized(
                    String(Int(metric.value(fromUsedPercent: $0.usedPercent).rounded())),
                    (metric == .used ? "used" : "remaining").localized)
            } ?? "No quota reading in this interval".localized)
                .font(.caption2)
                .foregroundStyle(.secondary)
            // What this interval actually cost, signed the way the card is
            // currently read: counting up it rises, counting down it falls.
            // Absent rather than zero when an end has no reading — an interval
            // nobody measured is not an interval that consumed nothing.
            if let delta = zone.consumed(metric) {
                Text("%@%@%% this interval".localized(
                    delta > 0 ? "+" : "", String(format: "%g", delta)))
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(delta == 0 ? AnyShapeStyle(.tertiary)
                                                : AnyShapeStyle(.primary))
            }
            if pending {
                Text("Reading local usage…".localized)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            } else {
                usageRows
            }
        }
        // Same chrome as the usage chart's tooltip, down to the radius and the
        // border style — two tooltips in one popover that differ by two points
        // of padding read as two different kinds of thing.
        .padding(8)
        .frame(width: 190, alignment: .leading)
        .tooltipSurface()
        // Same measurement path as the usage chart's tooltip: a background
        // GeometryReader reports one frame late, which is exactly the frame
        // the placement is computed in.
        .onGeometryChange(for: CGSize.self) { $0.size } action: { measuredSize = $0 }
    }

    @ViewBuilder private var usageRows: some View {
        HStack {
            // Paired with the amount beside it, so the same rule applies in
            // both directions: an interval of cost-only usage has a real
            // price and no token count, and "0 tokens · $4.10" reads as a
            // measurement rather than an absence.
            Text("%@ tokens".localized(Format.tokens(
                tokens: total, cost: messages.reduce(0) { $0 + $1.cost })))
            Spacer()
            // `money`: an all-unpriced interval sums to exactly zero,
            // which `usdOrBelowCent` renders "$0.00" — it only covers the
            // sub-cent half. Per-interval this is the likelier case of the
            // two, not the rarer one.
            Text(Format.money(
                tokens: total, cost: messages.reduce(0) { $0 + $1.cost }))
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
        ForEach(kinds, id: \.label) { kind in
            HStack(spacing: 4) {
                RoundedRectangle(cornerRadius: 1.5)
                    .fill(Color(hex: kind.color))
                    .frame(width: 6, height: 6)
                Text(kind.label.localized)
                Spacer()
                Text("\(Format.compactTokens(kind.value)) · "
                     + "\(Int((Double(kind.value) / Double(max(1, total)) * 100).rounded()))%")
                    .foregroundStyle(.secondary)
            }
            .font(.caption2)
        }
        if kinds.isEmpty {
            Text("No usage in this interval".localized)
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
    }
}

/// Owns the window chart's two animations under the glass panel. The
/// remaining/used toggle mirrors every point (`y` becomes `100 - y`), so the
/// line eases point by point. Switching to another window swaps in another
/// sample set on another time span, so the line keeps its drawn shape and
/// stretches or shrinks into the new one; see `MorphingQuotaCanvas`.
private struct QuotaChartStage: View {
    let geo: ChartGeometry
    /// The card id `geo` was drawn for — the window on screen, which changes
    /// in the same update as `geo`. Not the stored selection: that can name a
    /// window the card fell back from, and it changes before `geo` arrives.
    let window: String
    let asUsed: Bool
    let draw: (GraphicsContext, ChartGeometry) -> Void

    @Environment(\.inGlassPanel) private var inGlassPanel
    @State private var from: ChartGeometry?
    @State private var drawnWindow: String?
    @State private var spanProgress: Double = 1

    var body: some View {
        MorphingQuotaCanvas(
            usedAmount: asUsed ? 1 : 0, spanProgress: spanProgress,
            from: from, geo: geo, geoIsUsed: asUsed, draw: draw)
            .panelSwitchAnimation(asUsed)
            .onAppear { drawnWindow = window }
            .onChange(of: geo) { old, _ in
                // New samples arrive on every poll; only a window switch
                // morphs the span.
                defer { drawnWindow = window }
                guard inGlassPanel, drawnWindow != nil, drawnWindow != window else { return }
                from = old
                spanProgress = 0
                // Next turn: set and animated in one update would coalesce
                // into a single no-op change from 1 to 1.
                DispatchQueue.main.async {
                    withAnimation(.easeInOut(duration: GlassPanelStyle.spanMorph)) {
                        spanProgress = 1
                    }
                }
            }
    }
}

/// The window chart's Canvas, drawing `geo` with its quota line eased two
/// ways. `usedAmount` (0 = remaining, 1 = used) mirrors each point's y, which
/// is exact because the toggle moves nothing else. `spanProgress` (0 = the
/// previous window as drawn, 1 = `geo`) resamples both lines to one point
/// count and interpolates positions, so the old line stretches or shrinks into
/// the new one; dots slide along the old line to their new places. Bars and
/// hit zones belong to the new window at once. Without an animation in the
/// transaction (outside the panel) both parameters land in one frame.
private struct MorphingQuotaCanvas: View, Animatable {
    var usedAmount: Double
    var spanProgress: Double
    let from: ChartGeometry?
    let geo: ChartGeometry
    let geoIsUsed: Bool
    let draw: (GraphicsContext, ChartGeometry) -> Void

    nonisolated var animatableData: AnimatablePair<Double, Double> {
        get { AnimatablePair(usedAmount, spanProgress) }
        set {
            usedAmount = newValue.first
            spanProgress = newValue.second
        }
    }

    var body: some View {
        Canvas { ctx, _ in draw(ctx, blended) }
    }

    private var blended: ChartGeometry {
        let target = mirrored(geo)
        guard let from, spanProgress < 1 else { return target }
        let source = mirrored(from)
        let t = spanProgress
        func lerp(_ a: Double, _ b: Double) -> Double { a + (b - a) * t }
        func lerp(_ a: CurvePoint, _ b: CurvePoint) -> CurvePoint {
            CurvePoint(x: lerp(a.x, b.x), y: lerp(a.y, b.y))
        }
        // A new window with fewer than two points has no line to grow into.
        let count = max(source.curve.count, target.curve.count)
        let curve = target.curve.count < 2 ? target.curve : (0..<count).compactMap { i in
            let at = Double(i) / Double(count - 1)
            guard let end = Self.point(on: target.curve, at: at) else { return nil }
            return lerp(Self.point(on: source.curve, at: at) ?? end, end)
        }
        let dots = target.samplePoints.enumerated().map { i, dot in
            let at = target.samplePoints.count < 2
                ? 1 : Double(i) / Double(target.samplePoints.count - 1)
            return lerp(Self.point(on: source.samplePoints, at: at) ?? dot, dot)
        }
        return ChartGeometry(
            nowX: lerp(source.nowX, target.nowX),
            firstSampleX: lerp(source.firstSampleX, target.firstSampleX),
            bars: target.bars, hits: target.hits, samplePoints: dots, curve: curve)
    }

    /// `geo`'s quota points eased toward the used reading by `usedAmount`.
    private func mirrored(_ geo: ChartGeometry) -> ChartGeometry {
        func morph(_ points: [CurvePoint]) -> [CurvePoint] {
            points.map { point in
                let remaining = geoIsUsed ? 100 - point.y : point.y
                return CurvePoint(x: point.x, y: remaining + usedAmount * (100 - 2 * remaining))
            }
        }
        return ChartGeometry(
            nowX: geo.nowX, firstSampleX: geo.firstSampleX, bars: geo.bars, hits: geo.hits,
            samplePoints: morph(geo.samplePoints), curve: morph(geo.curve))
    }

    /// The point a fraction `at` (0...1) of the way along `points` by index.
    private static func point(on points: [CurvePoint], at: Double) -> CurvePoint? {
        guard let first = points.first else { return nil }
        guard points.count > 1 else { return first }
        let position = at * Double(points.count - 1)
        let i = min(Int(position), points.count - 2)
        let f = position - Double(i)
        let a = points[i], b = points[i + 1]
        return CurvePoint(x: a.x + (b.x - a.x) * f, y: a.y + (b.y - a.y) * f)
    }
}
