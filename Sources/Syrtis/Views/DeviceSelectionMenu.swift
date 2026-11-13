import SwiftUI
import TokenBarCore

// SA-3d: the device menu beside the year menu, and the lens that shows the
// core's merged pages for the selected devices. Nothing here adds usage
// across sources: it renders one overlay page at a time.

/// Copy for the device overlay. English is the catalog key. Text that comes
/// from another device (a label, a fingerprint) is only ever a format argument
/// or `Text(verbatim:)`, never a key.
enum DeviceOverlayCopy {
    static let menuHelp = "Choose which devices to include"
    static let thisMac = "This Mac"
    static let deviceCount = "%lld devices"
    static let otherDevices = "Other devices"
    static let loading = "Loading usage from the selected devices…"
    static let failedNoData = "Couldn't load usage from the selected devices."
    static let keptLast = "Couldn't update from the selected devices. Showing the last data received."
    static let modelsHidden = "Model details aren't available yet for the selected devices."
    static let lastGood = "%@ didn't respond. Showing older data (last updated %@)."
    static let stale = "%@: showing older data. The time of its last update isn't known."
    static let unavailable = "%@ has no data to show."
    static let refused = "%@ refused the request. Check Devices in Settings."
    static let invalidReply = "%@ sent a reply Syrtis couldn't use."
    static let incompatible = "%@ can't answer this request. It may need an update."
    static let duplicate = "Devices that sync their usage history can be counted more than once."
    static let saturated = "Some totals reached their limit and are capped."
    static let rangeLimited = "For other devices, All years covers only the last 370 days."

    static let all = [
        menuHelp, thisMac, deviceCount, otherDevices, loading, failedNoData, keptLast,
        modelsHidden, lastGood, stale, unavailable, refused, invalidReply, incompatible,
        duplicate, saturated, rangeLimited,
    ]

    /// The state's template; the same fixed line for a state this build does not know.
    static func template(forState state: String) -> String? {
        switch state {
        case "local", "fresh": nil
        case "lastGood": lastGood
        case "stale": stale
        case "refused": refused
        case "invalidReply": invalidReply
        case "incompatible": incompatible
        default: unavailable
        }
    }

    /// What names a device in a line: its `host:port` label, else a fixed
    /// word for the unreadable-peer-list row, else its fingerprint.
    static func name(of device: RemoteDevice) -> String {
        device.label ?? (device.id == "peers" ? otherDevices.localized : device.id)
    }

    /// The lines under a lens for pages the core answered: one per device
    /// that is neither this Mac nor fresh, then the three flags. Deduplicated
    /// so Stats, which shows two pages, does not say a thing twice.
    static func lines(
        _ pages: [(devices: [RemoteDevice], duplicate: Bool, saturated: Bool, rangeLimited: Bool)],
        now: Date = Date()
    ) -> [String] {
        var out: [String] = []
        func add(_ line: String) { if !out.contains(line) { out.append(line) } }
        for page in pages {
            for device in page.devices {
                guard let template = template(forState: device.state) else { continue }
                let name = name(of: device)
                if device.state == "lastGood", let at = device.lastSuccessAt, at >= 0 {
                    add(template.localized(name, Format.relativeTime(UInt64(at), now: now)))
                } else if device.state == "lastGood" {
                    add(stale.localized(name))
                } else {
                    add(template.localized(name))
                }
            }
            if page.duplicate { add(duplicate.localized) }
            if page.saturated { add(saturated.localized) }
            if page.rangeLimited { add(rangeLimited.localized) }
        }
        return out
    }
}

/// The device menu, shown only when a successful peer list has at least one
/// active device.
struct DeviceSelectionMenu: View {
    let overlay: DeviceOverlayState

    private var title: String {
        if overlay.selectedCount == 1, !overlay.stored.local,
           let peer = overlay.selectablePeers.first(where: { overlay.isSelected(peer: $0.producerStatic) }) {
            return "\(peer.host):\(peer.port)"
        }
        if overlay.selectedCount == 1 { return DeviceOverlayCopy.thisMac.localized }
        return DeviceOverlayCopy.deviceCount.localized(overlay.selectedCount)
    }

    var body: some View {
        if !overlay.selectablePeers.isEmpty {
            Menu {
                Toggle(isOn: Binding(
                    get: { overlay.stored.local }, set: { _ in overlay.toggleLocal() })
                ) {
                    Text(DeviceOverlayCopy.thisMac.localized)
                }
                .disabled(!overlay.canToggle(local: true))
                ForEach(overlay.selectablePeers, id: \.producerStatic) { peer in
                    Toggle(isOn: Binding(
                        get: { overlay.isSelected(peer: peer.producerStatic) },
                        set: { _ in overlay.togglePeer(peer.producerStatic) })
                    ) {
                        Text(verbatim: "\(peer.host):\(peer.port)")
                    }
                    .disabled(!overlay.canToggle(local: false, peer: peer.producerStatic))
                }
            } label: {
                Text(verbatim: title)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.visible)
            .fixedSize()
            .help(DeviceOverlayCopy.menuHelp.localized)
        }
    }
}

/// The four overlay lenses (Graph: Stats, Daily, Monthly; Models; Hourly;
/// Agents) while devices other than this Mac are selected. Shows only what the
/// core returned for the selection, with the per-device notes above it. While
/// the page is not there yet it shows a fixed line, never this Mac's own data
/// presented as the selection's.
struct DeviceOverlayLens: View {
    let overlay: DeviceOverlayState
    let view: AppView
    let singleClient: String?
    let hiddenRaw: String
    let orderRaw: String
    let year: String?
    /// This Mac's colors, for lenses without a models overlay of their own: a
    /// model only a remote device used gets the provider's base shade there.
    let localColors: ModelColorMap
    let onExpand: () -> Void

    /// Gap between the note lines and the lens below them.
    private static let noteSpacing: CGFloat = 8

    private var clientsArgument: [String]? {
        DeviceOverlayState.clientsArgument(
            singleClient: singleClient, hidden: ClientRegistry.parseIdSet(hiddenRaw))
    }

    private func page<P>(
        _ slot: DeviceOverlaySlot<P>?, _ kind: DeviceOverlayKind
    ) -> (page: DeviceAwarePayload<P>?, failed: Bool) {
        guard let slot, slot.key == overlay.key(kind, year: year, clients: clientsArgument)
        else { return (nil, false) }
        return (slot.data, slot.failed)
    }

    var body: some View {
        let graph = page(overlay.graph, .graph)
        let models = page(overlay.models, .models)
        let hourly = page(overlay.hourly, .hourly)
        let agents = page(overlay.agents, .agents)
        VStack(alignment: .leading, spacing: Self.noteSpacing) {
            switch view {
            case .models:
                shown(models.page, failed: models.failed, notes: [note(models.page)]) { report in
                    ModelsView(
                        report: report.payload,
                        clientIds: overlay.displayClients(
                            .models, singleClient: singleClient, hiddenRaw: hiddenRaw, orderRaw: orderRaw),
                        colors: overlay.modelsColors, loading: false)
                }
            case .daily, .monthly, .stats:
                shown(
                    graph.page, failed: graph.failed || (view == .stats && models.failed),
                    notes: view == .stats ? [note(graph.page), note(models.page)] : [note(graph.page)]
                ) { page in
                    let ids = overlay.displayClients(
                        .graph, singleClient: singleClient, hiddenRaw: hiddenRaw, orderRaw: orderRaw)
                    switch view {
                    case .daily:
                        DailyView(
                            payload: page.payload, clientIds: ids,
                            turnClientIds: PopoverView.supportedTurnClients(ids),
                            colors: localColors, onExpand: onExpand)
                    case .monthly:
                        MonthlyView(
                            payload: page.payload, clientIds: ids,
                            turnClientIds: PopoverView.supportedTurnClients(ids),
                            colors: localColors, onExpand: onExpand)
                    default:
                        if let stats = overlay.stats(selecting: Set(ids)) {
                            let modelPart = overlay.statsModels(year: year)
                            StatsView(
                                payload: page.payload, clientIds: ids, stats: stats,
                                modelReport: modelPart.report, modelYear: year ?? "",
                                colors: modelPart.report == nil ? localColors : overlay.modelsColors,
                                year: year, singleClient: singleClient, reportLoading: false,
                                modelsUnavailableNote: modelPart.note)
                        }
                    }
                }
            case .hourly:
                shown(hourly.page, failed: hourly.failed, notes: [note(hourly.page)]) { report in
                    HourlyView(
                        report: report.payload,
                        clientIds: overlay.displayClients(
                            .hourly, singleClient: singleClient, hiddenRaw: hiddenRaw, orderRaw: orderRaw))
                }
            case .agents:
                shown(agents.page, failed: agents.failed, notes: [note(agents.page)]) { report in
                    AgentsView(
                        report: report.payload,
                        clientIds: overlay.displayClients(
                            .agents, singleClient: singleClient, hiddenRaw: hiddenRaw, orderRaw: orderRaw))
                }
            case .overview, .quota:
                EmptyView()
            }
        }
    }

    private typealias Note = (devices: [RemoteDevice], duplicate: Bool, saturated: Bool, rangeLimited: Bool)

    private func note<P>(_ page: DeviceAwarePayload<P>?) -> Note? {
        page.map { ($0.devices, $0.duplicateWarning, $0.saturated, $0.remoteRangeLimited) }
    }

    /// The page with its notes above it, or the fixed line for a page that is
    /// not there: loading, or a refresh that failed with nothing to keep.
    @ViewBuilder private func shown<P, Content: View>(
        _ page: DeviceAwarePayload<P>?, failed: Bool, notes: [Note?],
        @ViewBuilder content: (DeviceAwarePayload<P>) -> Content
    ) -> some View {
        if let page {
            let lines = DeviceOverlayCopy.lines(notes.compactMap { $0 })
                + (failed ? [DeviceOverlayCopy.keptLast.localized] : [])
            ForEach(lines, id: \.self) { line in
                Text(verbatim: line)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            content(page)
        } else if failed {
            Text(DeviceOverlayCopy.failedNoData.localized)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, minHeight: 80)
        } else {
            LoadingLine(title: DeviceOverlayCopy.loading)
                .frame(maxWidth: .infinity, minHeight: 80)
        }
    }
}
