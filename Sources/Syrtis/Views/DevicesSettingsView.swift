import SwiftUI
import TokenBarCore

/// The Settings "Devices" page (SA-3c). Built from `SettingsPanel`'s own
/// layout helpers (`panel`), so it looks like the other pages; every action
/// goes through `DevicesModel`, which only calls `TBCore.remote*`.
struct DevicesSettingsView: View {
    /// The page's sizes. Everything else is the shared Settings helpers.
    private enum Layout {
        /// Text fields sit in a `row`'s trailing side; the content column is
        /// 354 pt wide (`SettingsWindowView`), so this leaves room for the
        /// row's label.
        static let fieldWidth: CGFloat = 170
        /// A SAS code is compared digit by digit with the other screen:
        /// monospaced and one step above the caption text around it.
        static let sasFont = Font.system(.callout, design: .monospaced).weight(.semibold)
        /// Space between the sub-blocks inside a section, the same as
        /// `section`'s own spacing.
        static let blockSpacing: CGFloat = 6
    }

    let panel: SettingsPanel
    @ObservedObject private var model = DevicesModel.shared

    @State private var host = ""
    @State private var portText = ""
    @State private var nodeName = ""
    /// Fields are filled from the first running status, once per page.
    @State private var seeded = false
    @State private var addresses: [String] = []
    /// The pasted invitation; handed to the model on Link and cleared.
    @State private var pasted = ""
    @State private var revokeTarget: RemoteGrant?
    @State private var removeTarget: RemotePeer?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            shareSection()
            if model.running {
                scopeSection()
                linkedSection()
            }
        }
        .onAppear {
            addresses = ListenAddress.choices(from: ListenAddress.interfaceAddresses())
            model.pageAppeared()
        }
        .onDisappear { model.pageDisappeared() }
        .onReceive(model.$status) { status in
            guard !seeded, let status, status.running else { return }
            host = status.bindHost ?? ""
            portText = status.bindPort.map(String.init) ?? ""
            nodeName = status.nodeLabel ?? ""
            seeded = true
        }
        .alert(
            DevicesCopy.revokeTitle.localized(revokeTarget?.peerFingerprint ?? ""),
            isPresented: Binding(get: { revokeTarget != nil }, set: { if !$0 { revokeTarget = nil } }),
            presenting: revokeTarget
        ) { grant in
            Button(DevicesCopy.revoke.localized, role: .destructive) {
                Task { await model.revoke(grant) }
            }
            Button(DevicesCopy.cancel.localized, role: .cancel) {}
        } message: { _ in
            Text(DevicesCopy.revokeMessage.localized)
        }
        .alert(
            DevicesCopy.removeTitle.localized(removeTarget.map(Self.address) ?? ""),
            isPresented: Binding(get: { removeTarget != nil }, set: { if !$0 { removeTarget = nil } }),
            presenting: removeTarget
        ) { peer in
            Button(DevicesCopy.remove.localized, role: .destructive) {
                Task { await model.removePeer(peer) }
            }
            Button(DevicesCopy.cancel.localized, role: .cancel) {}
        } message: { _ in
            Text(DevicesCopy.removeMessage.localized)
        }
    }

    // MARK: (A) Share this Mac

    @ViewBuilder
    private func shareSection() -> some View {
        panel.infoSection(
            DevicesCopy.shareSection, summary: DevicesCopy.shareSummary,
            details: {
                panel.hint(DevicesCopy.shareOffNote)
                panel.hint(DevicesCopy.approveEffect)
            }
        ) {
            panel.row(DevicesCopy.service) {
                Text((model.running ? DevicesCopy.serviceRunning : DevicesCopy.serviceStopped).localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if model.showsRetry {
                notice(model.startFailure ?? DevicesCopy.notRunning)
                Button(DevicesCopy.retry.localized) { Task { await model.retryStart() } }
                    .controlSize(.small)
                    .disabled(model.working)
                    .padding(.horizontal, 10)
            } else {
                sharingControls()
                addressControls()
                invitationControls()
                requestRows()
                grantRows()
            }
        }
    }

    @ViewBuilder
    private func sharingControls() -> some View {
        panel.toggleRow(
            DevicesCopy.shareToggle,
            isOn: Binding(
                get: { model.sharingOn },
                set: { next in Task { await model.setSharing(next) } }))
            .disabled(!model.canToggleSharing)
        panel.hint(DevicesCopy.shareOffNote)
        if !model.scopeApproved, !model.sharingOn { notice(DevicesCopy.needsScope) }
        if let line = model.sharingError { notice(line) }
    }

    @ViewBuilder
    private func addressControls() -> some View {
        panel.row(DevicesCopy.deviceName) {
            TextField("", text: $nodeName)
                .textFieldStyle(.roundedBorder)
                .font(.caption)
                .frame(width: Layout.fieldWidth)
        }
        caption(DevicesCopy.listenOn)
        panel.radioGroup(
            selection: $host,
            options: addresses.map { address in
                (address, address == ListenAddress.localOnly ? DevicesCopy.localOnlyLabel : address)
            })
        panel.row(DevicesCopy.port) {
            TextField("", text: $portText)
                .textFieldStyle(.roundedBorder)
                .font(.caption.monospacedDigit())
                .frame(width: Layout.fieldWidth)
        }
        Button(DevicesCopy.apply.localized) {
            let (host, port, name) = (host, Int(portText) ?? 0, nodeName)
            Task { await model.applyListen(host: host, port: port, nodeLabel: name) }
        }
        .controlSize(.small)
        .disabled(host.isEmpty)
        .padding(.horizontal, 10)
        if model.unreachable { notice(DevicesCopy.unreachableLine) }
        if ListenAddress.mayBePublic(host) { notice(DevicesCopy.publicAddress) }
        if let line = model.listenError { notice(line) }
    }

    @ViewBuilder
    private func invitationControls() -> some View {
        if let offer = model.offer {
            panel.hint(DevicesCopy.invitationReady)
            TimelineView(.periodic(from: .now, by: 1)) { context in
                Text(DevicesCopy.expiresIn.localized(Self.remaining(until: offer.expiresAt, now: context.date)))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 10)
            HStack {
                Button(DevicesCopy.copy.localized) { model.copyOffer() }
                Button(DevicesCopy.cancelInvitation.localized) { model.clearOffer() }
                if model.copied {
                    Text(DevicesCopy.copiedLine.localized)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            .controlSize(.small)
            .padding(.horizontal, 10)
        } else {
            Button(DevicesCopy.createInvitation.localized) { Task { await model.createOffer() } }
                .controlSize(.small)
                .disabled(!model.sharingOn)
                .padding(.horizontal, 10)
        }
        if let line = model.offerError { notice(line) }
    }

    @ViewBuilder
    private func requestRows() -> some View {
        if !model.pending.isEmpty {
            caption(DevicesCopy.requests)
            // Identity is the pending id; each button sends its own row's id
            // (the one shown with this SAS), never an index.
            ForEach(model.pending, id: \.pendingId) { request in
                VStack(alignment: .leading, spacing: Layout.blockSpacing) {
                    HStack {
                        Text(request.sasCode)
                            .font(Layout.sasFont)
                            .textSelection(.enabled)
                        Spacer()
                        Button(DevicesCopy.reject.localized) {
                            Task { await model.decide(request, approve: false) }
                        }
                        Button(DevicesCopy.approve.localized) {
                            Task { await model.decide(request, approve: true) }
                        }
                    }
                    .controlSize(.small)
                    .disabled(model.deciding.contains(request.pendingId))
                    panel.hint(DevicesCopy.compareCode)
                    panel.hint(DevicesCopy.approveEffect)
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 7)
                .glassCard(cornerRadius: 8)
            }
        }
        if let line = model.decideError { notice(line) }
    }

    @ViewBuilder
    private func grantRows() -> some View {
        let active = model.grants.filter { $0.state == "active" }
        if !active.isEmpty {
            caption(DevicesCopy.grantsHeader)
            ForEach(active, id: \.grantId) { grant in
                panel.row(grant.peerFingerprint) {
                    Text(DevicesCopy.expiresOn.localized(Self.date(grant.expiresAt)))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Button(DevicesCopy.revoke.localized) { revokeTarget = grant }
                        .controlSize(.small)
                }
            }
        }
        if let line = model.manageError { notice(line) }
    }

    // MARK: (B) Scan scope

    @ViewBuilder
    private func scopeSection() -> some View {
        panel.section(DevicesCopy.scopeSection) {
            panel.hint(DevicesCopy.scopeSummary)
            panel.hint(DevicesCopy.scopeBody)
            if let disclosure = model.disclosure {
                pathList(DevicesCopy.homeFolder, [disclosure.home])
                pathList(DevicesCopy.configFolder, [disclosure.configDir])
                pathList(DevicesCopy.dataFolder, [disclosure.dataDir])
                pathList(DevicesCopy.addedFolders, Self.flatten(disclosure.extraScanPaths))
                pathList(DevicesCopy.excludedFolders, Self.flatten(disclosure.excludedScanPaths))
                pathList(DevicesCopy.openCodeDatabases, disclosure.opencodeDbPaths)
                pathList(DevicesCopy.clients, [disclosure.clients.joined(separator: ", ")])
                if disclosure.envRootsSet {
                    notice(DevicesCopy.envRoots.localized(Self.variableNames(disclosure.envRoots)))
                }
            }
            if model.scopeApproved {
                panel.hint(DevicesCopy.scopeApprovedLine)
            } else {
                notice(DevicesCopy.needsApproval)
            }
            // The digest of the disclosure this button was rendered with.
            let shownDigest = model.disclosure?.ownerInputsSha256
            Button(DevicesCopy.approveScope.localized) {
                guard let shownDigest else { return }
                Task { await model.approveScope(digest: shownDigest) }
            }
                .controlSize(.small)
                .disabled(model.disclosure == nil || model.scopeApproved)
                .padding(.horizontal, 10)
            if let line = model.scopeNotice { notice(line) }
        }
    }

    /// A labelled list in a card; empty lists are left out.
    @ViewBuilder
    private func pathList(_ label: String, _ items: [String]) -> some View {
        if !items.isEmpty, items != [""] {
            VStack(alignment: .leading, spacing: 2) {
                Text(label.localized)
                    .font(.caption)
                ForEach(items, id: \.self) { item in
                    Text(verbatim: item)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .glassCard(cornerRadius: 8)
        }
    }

    // MARK: (C) Linked devices

    @ViewBuilder
    private func linkedSection() -> some View {
        panel.infoSection(
            DevicesCopy.linkedSection, summary: DevicesCopy.linkedSummary,
            details: { panel.hint(DevicesCopy.removeMessage) }
        ) {
            if model.peers.isEmpty { panel.hint(DevicesCopy.noLinked) }
            ForEach(model.peers, id: \.producerStatic) { peer in
                panel.row(Self.address(peer)) {
                    Text(DevicesCopy.state(peer.state).localized)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Button(DevicesCopy.remove.localized) { removeTarget = peer }
                        .controlSize(.small)
                }
                panel.hint("\(peer.fingerprint) · " + DevicesCopy.expiresOn.localized(Self.date(peer.expiresAt)))
                    .padding(.horizontal, 10)
            }
            caption(DevicesCopy.linkAnother)
            linkControls()
        }
    }

    @ViewBuilder
    private func linkControls() -> some View {
        switch model.pairing {
        case .waiting(let claim):
            Text(DevicesCopy.yourCode.localized(claim.sas))
                .font(Layout.sasFont)
                .textSelection(.enabled)
                .padding(.horizontal, 10)
            panel.hint(DevicesCopy.waitingApproval)
            Button(DevicesCopy.cancel.localized) { model.cancelWait() }
                .controlSize(.small)
                .padding(.horizontal, 10)
        case .claiming:
            ProgressView().controlSize(.small).padding(.horizontal, 10)
        case .idle, .linked, .failed:
            HStack {
                // Masked: the invitation is a secret until it is claimed.
                SecureField(DevicesCopy.pastePlaceholder.localized, text: $pasted)
                    .textFieldStyle(.roundedBorder)
                    .font(.caption)
                Button(DevicesCopy.link.localized) {
                    let text = pasted
                    pasted = ""
                    Task { await model.link(pairUrl: text) }
                }
                .controlSize(.small)
                .disabled(pasted.isEmpty)
            }
            .padding(.horizontal, 10)
            if case .linked = model.pairing { panel.hint(DevicesCopy.linkedLine) }
            if case .failed(let line) = model.pairing { notice(line) }
        }
    }

    // MARK: Pieces

    /// A sub-heading inside a section, the style `AccountsCopy.autoToggle`
    /// uses inside its popover.
    private func caption(_ key: String) -> some View {
        Text(key.localized)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 10)
    }

    /// A state notice: the orange caption the Accounts page uses for one.
    private func notice(_ key: String) -> some View {
        Text(key.localized)
            .font(.caption2)
            .foregroundStyle(.orange)
            .fixedSize(horizontal: false, vertical: true)
    }

    static func address(_ peer: RemotePeer) -> String {
        peer.host.contains(":") ? "[\(peer.host)]:\(peer.port)" : "\(peer.host):\(peer.port)"
    }

    /// Each variable once, in the order given: several clients can follow
    /// the same variable (all of tokscale's own roots follow
    /// `TOKSCALE_CONFIG_DIR`), and the notice listed it once per client.
    static func variableNames(_ roots: [RemoteScopeDisclosure.EnvRoot]) -> String {
        var seen = Set<String>()
        return roots.map(\.variable).filter { seen.insert($0).inserted }.joined(separator: ", ")
    }

    static func flatten(_ paths: [String: [String]]) -> [String] {
        paths.keys.sorted().flatMap { client in paths[client, default: []].map { "\(client): \($0)" } }
    }

    static func date(_ unixMs: Int64) -> String {
        Date(timeIntervalSince1970: TimeInterval(unixMs) / 1000)
            .formatted(date: .abbreviated, time: .shortened)
    }

    static func remaining(until unixMs: Int64, now: Date) -> String {
        let seconds = max(0, Int((TimeInterval(unixMs) / 1000 - now.timeIntervalSince1970).rounded(.up)))
        return String(format: "%d:%02d", seconds / 60, seconds % 60)
    }
}
