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
    static let lastGood = "%@: no new data this time. Showing older data (last updated %@)."
    static let stale = "%@: showing older data. The time of its last update isn't known."
    static let unavailable = "%@ has no data to show."
    static let overLimit = "%@ has too much data to include."
    static let refused = "%@ refused the request. Check Devices in Settings."
    static let invalidReply = "%@ sent a reply Syrtis couldn't use."
    static let incompatible = "%@ can't answer this request. It may need an update."
    static let duplicate = "Devices that sync their usage history can be counted more than once."
    static let saturated = "Some totals reached their limit and are capped."
    static let rangeLimited = "For other devices, All years covers only the last 370 days."
    static let aboutCombined = "About combined usage"

    // DR-2a: the status line (one sentence per device that is not fresh), and
    // the labels that say which numbers are combined and which are this Mac's.
    static let gettingUsage = "◌ Getting usage from %@…"
    // DR-2b2: a device that has never answered this Mac scans its history first.
    static let gettingUsageFirstScan =
        "◌ Getting usage from %@… The first time needs to scan its records, about 1 minute"
    static let costIncomplete =
        "The cost for %@ may be incomplete (its price data is incomplete). Tokens and message counts are included in the totals."
    static let unreachableData =
        "Can't reach %@. Make sure it has Syrtis open and is on the same network. Showing data from %@."
    static let unreachableNoData =
        "No data from %@ yet. Can't reach it. Make sure it has Syrtis open and is on the same network."
    static let busyData = "%@ is busy for now. Showing data from %@."
    static let busyDataRetry = "%@ is busy for now and will be retried in 1 minute. Showing data from %@."
    static let busyNoData = "No data from %@ yet. It is busy for now."
    static let busyNoDataRetry = "No data from %@ yet. It is busy for now and will be retried in 1 minute."
    static let stopped = "%@ stopped sharing. Reconnect it in Settings → Devices."
    static let errorData = "%@ couldn't answer this request. Showing data from %@."
    static let errorNoData = "No data from %@ yet. It couldn't answer this request."
    static let noDataYet = "No data from %@ yet."
    static let combined = "%lld devices combined"
    static let thisMacLive = "(this Mac)"
    static let quotaThisMacOnly = "Quota counts this Mac only"
    static let thisMacOnly = "This Mac only"
    static let windowUsageThisMacOnly = "Usage inside windows counts this Mac only"
    static let perDevice = "Per device"
    static let updatedAt = "Updated %@"

    // DR-2c: the short status, one phrase per device (see `shortStatus`).
    static let shortNotIncluded = "Not included"
    static let shortUpdating = "Updating…"
    static let shortConnectedAt = "Connected · updated %@"
    static let shortConnected = "Connected"
    static let shortStopped = "Stopped sharing"
    static let shortRefused = "Refused · check Devices in Settings"
    static let shortIncompatible = "Needs an update"
    static let shortUnusable = "Data can't be used"
    static let shortUnreachable = "Can't reach · %@"
    static let shortBusy = "Busy for now · %@"
    static let shortError = "Can't answer · %@"
    static let shortEarlier = "Showing earlier data"
    static let shortNoData = "No data yet"
    static let shortFrom = "Showing data from %@"
    static let tailFrom = "showing data from %@"
    static let tailEarlier = "showing earlier data"
    static let tailNone = "no data yet"

    static let all = [
        menuHelp, thisMac, deviceCount, otherDevices, loading, failedNoData, keptLast,
        modelsHidden, lastGood, stale, unavailable, overLimit, refused, invalidReply, incompatible,
        duplicate, saturated, rangeLimited, aboutCombined,
        gettingUsage, gettingUsageFirstScan, costIncomplete, unreachableData, unreachableNoData, busyData, busyDataRetry, busyNoData,
        busyNoDataRetry, stopped, errorData, errorNoData, noDataYet, combined, thisMacLive,
        quotaThisMacOnly, thisMacOnly, windowUsageThisMacOnly, perDevice, updatedAt,
        shortNotIncluded, shortUpdating, shortConnectedAt, shortConnected, shortStopped, shortRefused,
        shortIncompatible, shortUnusable, shortUnreachable, shortBusy, shortError, shortEarlier,
        shortNoData, shortFrom, tailFrom, tailEarlier, tailNone,
    ]

    /// What names a device in a line: the label the core gave (the owner's
    /// name for it, else its `host:port`), else a fixed word for the
    /// unreadable-peer-list row, else its fingerprint.
    static func name(of device: RemoteDevice) -> String {
        device.label ?? (device.id == "peers" ? otherDevices.localized : device.id)
    }

    /// The same for a linked device that has no `devices` row yet: the name it
    /// last sent, else the address it was linked at.
    static func name(of peer: RemotePeer) -> String {
        peer.nodeLabel ?? address(host: peer.host, port: peer.port)
    }

    /// The device menu's button: the one selected device's name, "This Mac",
    /// or a count.
    static func menuTitle(selectedCount: Int, local: Bool, peerName: String?) -> String {
        if selectedCount == 1, !local, let peerName { return peerName }
        if selectedCount == 1 { return thisMac.localized }
        return deviceCount.localized(selectedCount)
    }

    /// Least data first: a device that is `unavailable` on one page and
    /// `lastGood` on another gets the `unavailable` sentence only.
    private static let severity = ["unavailable", "overLimit", "refused", "incompatible", "invalidReply", "stale", "lastGood"]

    /// The sentences in the ⓘ popover beside the device menu: the two general
    /// notes about combining devices, which are not trouble and so are not
    /// lines under the lens. The range note only when the page reports it.
    static func explanations(rangeLimited: Bool) -> [String] {
        [duplicate.localized] + (rangeLimited ? [Self.rangeLimited.localized] : [])
    }

    /// The dot in front of a short status. Never the only signal: the text
    /// carries the state.
    enum ShortDot: Equatable { case green, orange, grey, progress }

    /// One device in one short phrase, no name: the single source for the
    /// device menu, the per-device card and Settings → linked devices.
    /// `device` is the device's row (the worst one when several pages list it;
    /// see `worstRow`), nil when no page has listed it. The conditions are
    /// tested in the order of the DR-2c table.
    static func shortStatus(
        device: RemoteDevice?, selected: Bool, inFlight: Bool, now: Date
    ) -> (dot: ShortDot, text: String) {
        if !selected { return (.grey, shortNotIncluded.localized) }
        if inFlight { return (.progress, shortUpdating.localized) }
        let at = device?.lastSuccessAt.flatMap { $0 >= 0 ? UInt64($0) : nil }
        let state = device?.state
        if state == "fresh" {
            guard let at else { return (.green, shortConnected.localized) }
            let age = Format.relativeTime(at, now: now)
            // A fresh row older than two polls (the popover is closed, so
            // nothing asks) no longer says the device is connected.
            if now.timeIntervalSince1970 - Double(at) > connectedFor {
                return (.grey, shortFrom.localized(age))
            }
            return (.green, shortConnectedAt.localized(age))
        }
        if device?.reason == "stopped" { return (.grey, shortStopped.localized) }
        if state == "refused" { return (.grey, shortRefused.localized) }
        if state == "incompatible" { return (.orange, shortIncompatible.localized) }
        if state == "invalidReply" || state == "overLimit" { return (.orange, shortUnusable.localized) }
        // Older data exists (lastGood, stale); how old is known only for a
        // lastGood with a time.
        let tail: String = {
            switch state {
            case "lastGood"?:
                return at.map { tailFrom.localized(Format.relativeTime($0, now: now)) } ?? tailEarlier.localized
            case "stale"?: return tailEarlier.localized
            default: return tailNone.localized
            }
        }()
        switch device?.reason {
        case "unreachable"?, "timeout"?: return (.orange, shortUnreachable.localized(tail))
        case "busy"?: return (.orange, shortBusy.localized(tail))
        case "error"?: return (.orange, shortError.localized(tail))
        default: break
        }
        if state == "lastGood", let at {
            return (.orange, shortFrom.localized(Format.relativeTime(at, now: now)))
        }
        if state == "stale" || state == "lastGood" { return (.orange, shortEarlier.localized) }
        return (.orange, shortNoData.localized)
    }

    /// How long a fresh answer still reads as "Connected": two polls of the
    /// open popover (`DeviceOverlayState.remotePollSeconds`, 300 s). An older
    /// answer means nothing is asking (the popover is closed) and the device's
    /// state is not known.
    static let connectedFor: TimeInterval = 600

    /// Of the rows `rows` holds for device `id`, the one to describe: the
    /// worst non-fresh one by `severity` (the status line's rule), else the
    /// first row. Nil when no row is for `id`.
    static func worstRow(id: String, in rows: [RemoteDevice]) -> RemoteDevice? {
        let mine = rows.filter { $0.id == id }
        let bad = mine.filter { $0.state != "fresh" }
        return bad.min { (severity.firstIndex(of: $0.state) ?? 0) < (severity.firstIndex(of: $1.state) ?? 0) }
            ?? mine.first
    }

    /// The name shown for a linked device: the newest name a page reported
    /// (`learned`, by fingerprint), else the peer list's, else its address.
    static func displayName(of peer: RemotePeer, learned: [String: String]) -> String {
        learned[peer.fingerprint] ?? name(of: peer)
    }

    /// A producer's address as the core labels it: IPv6 hosts in brackets.
    static func address(host: String, port: Int) -> String {
        host.contains(":") ? "[\(host)]:\(port)" : "\(host):\(port)"
    }

    /// The status line: one sentence for each selected device that is not
    /// fresh, in the order the pages list them (selected devices that no page
    /// mentions yet follow, as "no data yet" or "getting usage"), each
    /// followed by its cost sentence when a page merged a body that said the
    /// device's price table was incomplete.
    ///
    /// `rows` are the `devices` rows of the pages the lens shows; a device on
    /// several pages gets its worst row, and one line per device id: two
    /// devices with the same name and state stay two lines. `inFlight` (a
    /// refresh is running) replaces every state sentence with "getting usage
    /// from X" (with the first-scan half-sentence when the core says X never
    /// answered); `retryPending` is whether a retry is actually armed, the
    /// only case that may say "again in a minute". Another device's name is
    /// only a format argument.
    static func statusLines(
        rows: [RemoteDevice], peers: [(id: String, name: String)], inFlight: Bool,
        retryPending: Bool, names: [String: String] = [:], now: Date = Date()
    ) -> [String] {
        /// `names` (the newest real name per device id) wins over a row's own
        /// label, which may come from an older page.
        func nameOf(_ d: RemoteDevice) -> String { names[d.id] ?? Self.name(of: d) }
        func rank(_ d: RemoteDevice) -> Int { severity.firstIndex(of: d.state) ?? 0 }
        // Per device id, in first-appearance order: its worst non-fresh row,
        // and whether a row with data says its cost is incomplete.
        var order: [String] = []
        var worst: [String: RemoteDevice] = [:]
        var costly: [String: RemoteDevice] = [:]
        // True when ANY visible row of the device says it has answered, whichever
        // row is the worst: Graph may have answered while Models is pending.
        var answered: [String: Bool] = [:]
        for row in rows where row.state != "local" {
            answered[row.id] = (answered[row.id] ?? false) || row.everAnswered
            if row.state != "fresh" {
                if let seen = worst[row.id] {
                    if rank(row) < rank(seen) { worst[row.id] = row }
                } else {
                    worst[row.id] = row
                }
            }
            if row.costIncomplete, ["fresh", "lastGood", "stale"].contains(row.state), costly[row.id] == nil {
                costly[row.id] = row
            }
            if (worst[row.id] != nil || costly[row.id] != nil), !order.contains(row.id) { order.append(row.id) }
        }
        // A selected device no page lists yet has had no answer.
        let listed = Set(rows.map(\.id))
        for peer in peers where !listed.contains(peer.id) {
            worst[peer.id] = RemoteDevice(
                id: peer.id, label: peer.name, state: "unavailable", lastSuccessAt: nil)
            order.append(peer.id)
        }
        var out: [String] = []
        for id in order {
            if let row = worst[id] {
                let device = RemoteDevice(
                    id: row.id, label: row.label, state: row.state, lastSuccessAt: row.lastSuccessAt,
                    reason: row.reason, totals: row.totals, address: row.address,
                    costIncomplete: row.costIncomplete, everAnswered: answered[id] ?? row.everAnswered)
                out.append(statusLine(
                    device, name: nameOf(device), inFlight: inFlight,
                    retryPending: retryPending, now: now))
            }
            if let device = costly[id] {
                out.append(costIncomplete.localized(nameOf(device)))
            }
        }
        return out
    }

    /// One device's sentence; the R3 table of the plan, cell by cell.
    static func statusLine(
        _ device: RemoteDevice, name: String, inFlight: Bool, retryPending: Bool, now: Date
    ) -> String {
        if inFlight {
            return device.everAnswered ? gettingUsage.localized(name) : gettingUsageFirstScan.localized(name)
        }
        let age: String? = {
            guard device.state == "lastGood", let at = device.lastSuccessAt, at >= 0 else { return nil }
            return Format.relativeTime(UInt64(at), now: now)
        }()
        switch (device.state, device.reason) {
        case ("stale", _):
            return stale.localized(name)
        case ("invalidReply", _): return invalidReply.localized(name)
        case ("overLimit", _): return overLimit.localized(name)
        case ("incompatible", _): return incompatible.localized(name)
        case (_, "unreachable"?), (_, "timeout"?):
            return age.map { unreachableData.localized(name, $0) } ?? unreachableNoData.localized(name)
        case (_, "busy"?):
            switch (age, retryPending) {
            case (let age?, true): return busyDataRetry.localized(name, age)
            case (let age?, false): return busyData.localized(name, age)
            case (nil, true): return busyNoDataRetry.localized(name)
            case (nil, false): return busyNoData.localized(name)
            }
        case (_, "stopped"?): return stopped.localized(name)
        case (_, "error"?):
            return age.map { errorData.localized(name, $0) } ?? errorNoData.localized(name)
        case ("refused", _): return refused.localized(name)
        case ("lastGood", _):
            return age.map { lastGood.localized(name, $0) } ?? stale.localized(name)
        case ("unavailable", _) where device.id == "peers":
            return unavailable.localized(name)
        default:
            return noDataYet.localized(name)
        }
    }

    /// The lines under a lens that are not about one device: the capped-totals
    /// warning, at most once across the pages.
    static func lines(
        _ pages: [(devices: [RemoteDevice], duplicate: Bool, saturated: Bool, rangeLimited: Bool)]
    ) -> [String] {
        pages.contains(where: \.saturated) ? [saturated.localized] : []
    }
}

/// What the overlay knows about its devices, in the shape the three places
/// that show a short status read it. The overlay publishes one to
/// `DevicesModel.shared` so Settings (a separate window with a model of its
/// own) shows what the dashboard last learned, and never anything of its own.
struct DeviceOverlaySnapshot: Equatable {
    /// The `devices` rows of the pages the overlay holds for the lens on screen.
    var rows: [RemoteDevice] = []
    /// Whether each listed device is selected, by fingerprint.
    var selection: [String: Bool] = [:]
    var inFlight = false
    var names: [String: String] = [:]

    func shortStatus(fingerprint: String, now: Date) -> (dot: DeviceOverlayCopy.ShortDot, text: String) {
        DeviceOverlayCopy.shortStatus(
            device: DeviceOverlayCopy.worstRow(id: fingerprint, in: rows),
            selected: selection[fingerprint] ?? false, inFlight: inFlight, now: now)
    }

    /// Settings shows nothing for a device this process has no result for
    /// (no state is made up), or one the dashboard does not offer.
    func settingsStatus(
        for peer: RemotePeer, now: Date
    ) -> (dot: DeviceOverlayCopy.ShortDot, text: String)? {
        guard let selected = selection[peer.fingerprint] else { return nil }
        if selected, DeviceOverlayCopy.worstRow(id: peer.fingerprint, in: rows) == nil { return nil }
        return shortStatus(fingerprint: peer.fingerprint, now: now)
    }
}

/// A short status: a small dot, then the phrase. The dot is a symbol (filled
/// for connected or trouble, hollow for not shown, dotted while updating) in
/// the state's colour, hidden from accessibility.
struct DeviceShortStatusView: View {
    let status: (dot: DeviceOverlayCopy.ShortDot, text: String)
    /// In a menu: AppKit draws a symbol as a template there and drops its
    /// colour (seen on screen: every dot grey), so the dot is a drawn,
    /// non-template image instead.
    var inMenu = false

    /// Side of the menu's drawn dot, the cap height of the menu's text.
    private static let menuDotSide: CGFloat = 9

    private static func menuDot(_ dot: DeviceOverlayCopy.ShortDot) -> NSImage {
        let side = menuDotSide
        let image = NSImage(size: NSSize(width: side, height: side), flipped: false) { rect in
            let circle = NSBezierPath(ovalIn: rect.insetBy(dx: 1, dy: 1))
            switch dot {
            case .green: NSColor.systemGreen.setFill(); circle.fill()
            case .orange: NSColor.systemOrange.setFill(); circle.fill()
            case .grey, .progress:
                NSColor.secondaryLabelColor.setStroke()
                circle.lineWidth = 1
                if dot == .progress { circle.setLineDash([1.5, 1.5], count: 2, phase: 0) }
                circle.stroke()
            }
            return true
        }
        image.isTemplate = false
        return image
    }

    /// Symbol and colour per dot. Orange is the colour of the other state
    /// notices in Settings; grey is `.secondary`, so a device the lens leaves
    /// out reads as quieter than one with trouble.
    private var symbol: (name: String, color: Color) {
        switch status.dot {
        case .green: ("circle.fill", .green)
        case .orange: ("circle.fill", .orange)
        case .grey: ("circle", .secondary)
        case .progress: ("circle.dotted", .secondary)
        }
    }

    var body: some View {
        // `Label` so a menu, which draws only a title and an icon, shows both.
        Label {
            Text(verbatim: status.text)
        } icon: {
            if inMenu {
                Image(nsImage: Self.menuDot(status.dot)).accessibilityHidden(true)
            } else {
                Image(systemName: symbol.name)
                    .foregroundStyle(symbol.color)
                    .imageScale(.small)
                    .accessibilityHidden(true)
            }
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
    }
}

/// The device menu's button text: one line, cut with an ellipsis past
/// `maxWidth`. The title can be another device's own name (up to 128 bytes).
struct DeviceMenuTitle: View {
    let title: String

    /// Wide enough for an address like `192.168.100.200:47000` or a usual
    /// device name, and narrow enough that the popover header (400 pt wide,
    /// with the wordmark, year menu and two buttons) keeps its other items
    /// when the name is long. Chosen for the header's layout, not measured there.
    static let maxWidth: CGFloat = 150

    var body: some View {
        Text(verbatim: title)
            .font(.caption.monospacedDigit())
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .truncationMode(.tail)
            .frame(maxWidth: Self.maxWidth)
    }
}

/// The device menu, shown only when a successful peer list has at least one
/// active device.
struct DeviceSelectionMenu: View {
    let overlay: DeviceOverlayState
    /// Whether the lens on screen reports the 370-day limit for other devices.
    let rangeLimited: Bool
    /// Whether the lens on screen has overlay pages: Overview and Quota show
    /// only this Mac, so the note about combining devices has nothing to explain.
    let lensHasOverlay: Bool
    @State private var infoShown = false

    /// Popover width for the two sentences; chosen for readable wrapping, not measured.
    private static let infoWidth: CGFloat = 260

    private var title: String {
        DeviceOverlayCopy.menuTitle(
            selectedCount: overlay.selectedCount, local: overlay.stored.local,
            peerName: overlay.selectablePeers.first { overlay.isSelected(peer: $0.producerStatic) }
                .map { overlay.displayName(of: $0) })
    }

    var body: some View {
        if !overlay.selectablePeers.isEmpty {
            HStack(spacing: 4) {
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
                        Text(verbatim: overlay.displayName(of: peer))
                    }
                    .disabled(!overlay.canToggle(local: false, peer: peer.producerStatic))
                    // Under the name: a disabled row, the only second line a menu has.
                    DeviceShortStatusView(
                        status: overlay.snapshot.shortStatus(fingerprint: peer.fingerprint, now: Date()),
                        inMenu: true)
                        .disabled(true)
                }
            } label: {
                DeviceMenuTitle(title: title)
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.visible)
            .fixedSize()
            .help(DeviceOverlayCopy.menuHelp.localized)
            if overlay.includesRemote && lensHasOverlay { infoButton }
            }
        }
    }

    private var infoButton: some View {
        let sentences = DeviceOverlayCopy.explanations(rangeLimited: rangeLimited)
        return Button { infoShown.toggle() } label: {
            Image(systemName: "info.circle")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .buttonStyle(.plain)
        .help(sentences.joined(separator: " "))
        .accessibilityLabel(DeviceOverlayCopy.aboutCombined.localized)
        .popover(isPresented: $infoShown, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 8) {
                ForEach(sentences, id: \.self) { Text(verbatim: $0) }
            }
            .font(.caption)
            .fixedSize(horizontal: false, vertical: true)
            .frame(width: Self.infoWidth, alignment: .leading)
            .padding(12)
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
    /// Gap between a lens and the per-device card under it: the cards' own
    /// stack spacing elsewhere in the popover.
    private static let blockSpacing: CGFloat = 12

    private var clientsArgument: [String]? {
        DeviceOverlayState.clientsArgument(
            singleClient: singleClient, hidden: ClientRegistry.parseIdSet(hiddenRaw))
    }

    private func page<P>(
        _ slot: DeviceOverlaySlot<P>?, _ kind: DeviceOverlayKind
    ) -> (page: DeviceAwarePayload<P>?, failed: Bool) {
        overlay.visible(slot, kind, year: year, clients: clientsArgument)
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
                        VStack(spacing: Self.blockSpacing) {
                            DailyView(
                                payload: page.payload, clientIds: ids,
                                turnClientIds: PopoverView.supportedTurnClients(ids),
                                colors: localColors, onExpand: onExpand)
                            DeviceTotalsCard(
                                devices: page.devices, clientIds: ids, names: overlay.learnedNames,
                                statusRows: overlay.snapshot.rows, inFlight: overlay.fetching)
                        }
                    case .monthly:
                        VStack(spacing: Self.blockSpacing) {
                            MonthlyView(
                                payload: page.payload, clientIds: ids,
                                turnClientIds: PopoverView.supportedTurnClients(ids),
                                colors: localColors, onExpand: onExpand)
                            DeviceTotalsCard(
                                devices: page.devices, clientIds: ids, names: overlay.learnedNames,
                                statusRows: overlay.snapshot.rows, inFlight: overlay.fetching)
                        }
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
            DevicesCombinedBadge(count: overlay.selectedCount)
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
