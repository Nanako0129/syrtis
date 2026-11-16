import SwiftUI
import TokenBarCore

// DR-2a: the "Per device" subtotals under Overview, Monthly and Daily, and the
// small label that says a card's numbers are combined. The numbers come from
// the core's `devices[].totals` (one entry per client); the only sum made
// here is over the entries of the clients the page shows.

enum DeviceTotals {
    struct Row: Equatable {
        let id: String
        /// This Mac, or another device's own text (only ever shown verbatim).
        let name: String
        let isLocal: Bool
        /// Nil when none of this device's part was merged into the page.
        let messages: Int64?
        let tokens: Int64?
        let cost: Double?
        /// Unix seconds of the last fresh pull; nil for this Mac.
        let updatedAt: Int64?
    }

    /// One row per device of the page. A device's figures are its core-computed
    /// per-client entries for `clientIds` added together, so they equal the
    /// combined card above them when that card shows the same clients.
    /// `names`: the newest real name per device id (`DeviceOverlayState.learnedNames`),
    /// which wins over the row's own label so the card agrees with the menu.
    static func rows(devices: [RemoteDevice], clientIds: [String], names: [String: String] = [:]) -> [Row] {
        let shown = Set(clientIds)
        return devices.map { device in
            let isLocal = device.id == "local"
            let parts = device.totals?.filter { shown.contains($0.client) }
            return Row(
                id: device.id,
                name: isLocal
                    ? DeviceOverlayCopy.thisMac.localized
                    : (names[device.id] ?? DeviceOverlayCopy.name(of: device)),
                isLocal: isLocal,
                messages: parts.map { list in list.reduce(Int64(0)) { sum, t in sum.saturatingAdd(t.messages) } },
                tokens: parts.map { list in list.reduce(Int64(0)) { sum, t in sum.saturatingAdd(t.tokens) } },
                cost: parts.map { list in list.reduce(0.0) { sum, t in sum + t.costUsd } },
                updatedAt: isLocal ? nil : device.lastSuccessAt)
        }
    }
}

private extension Int64 {
    func saturatingAdd(_ other: Int64) -> Int64 {
        let (sum, overflow) = addingReportingOverflow(other)
        return overflow ? (other < 0 ? .min : .max) : sum
    }
}

/// The "N devices combined" label, right-aligned above a combined card.
struct DevicesCombinedBadge: View {
    let count: Int

    var body: some View {
        Text(DeviceOverlayCopy.combined.localized(count))
            .font(.caption2)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

/// Per-device subtotals: name, messages, tokens, cost and when it was last
/// updated (this Mac has no time).
struct DeviceTotalsCard: View {
    let devices: [RemoteDevice]
    let clientIds: [String]
    var names: [String: String] = [:]
    var now: Date = Date()

    var body: some View {
        let rows = DeviceTotals.rows(devices: devices, clientIds: clientIds, names: names)
        DashCard(DeviceOverlayCopy.perDevice) {
            VStack(alignment: .leading, spacing: Self.rowSpacing) {
                ForEach(rows, id: \.id) { row in
                    HStack(alignment: .firstTextBaseline) {
                        VStack(alignment: .leading, spacing: 1) {
                            Text(verbatim: row.name)
                                .font(.caption.weight(.medium))
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Text(verbatim: Self.detail(row, now: now))
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 8)
                        Text(verbatim: row.cost.map(Format.usd) ?? "—")
                            .font(.caption.monospacedDigit())
                    }
                }
            }
        }
    }

    /// Gap between device rows; two lines of caption text each, so a little
    /// air keeps neighbouring rows from reading as one.
    private static let rowSpacing: CGFloat = 8

    static func detail(_ row: DeviceTotals.Row, now: Date) -> String {
        var parts: [String] = []
        if let messages = row.messages, let tokens = row.tokens {
            // Grouped like the Monthly rows: the same key, the same `.formatted()`.
            parts.append("%@ msgs".localized(messages.formatted()))
            parts.append("%@ tokens".localized(Format.compactTokens(tokens)))
        } else {
            parts.append("—")
        }
        if let at = row.updatedAt, at >= 0 {
            parts.append(DeviceOverlayCopy.updatedAt.localized(
                Format.relativeTime(UInt64(at), now: now)))
        }
        return parts.joined(separator: " · ")
    }
}

/// The status line under the tab row: the sentences of
/// `DeviceOverlayCopy.statusLines`, or nothing when every device is fresh.
struct DeviceStatusLine: View {
    let lines: [String]

    var body: some View {
        if !lines.isEmpty {
            VStack(alignment: .leading, spacing: 2) {
                // By position: two devices with the same name and state read the same.
                ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                    Text(verbatim: line)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
