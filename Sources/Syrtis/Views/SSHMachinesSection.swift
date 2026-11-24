import SwiftUI

/// The Devices page's "SSH machines" section: one line per machine (state dot,
/// address, status, a ⋯ menu), an inline add / consent / remove card, and only what needs the owner under a row. Built from the page's own
/// `SettingsPanel` helpers; every action goes through `SSHMachinesModel`.
/// Text that came from the machine (labels, disclosure, ssh's own lines) is
/// `Text(verbatim:)`, never a localization key.
struct SSHMachinesSection: View {
    private enum Layout {
        static let fieldWidth: CGFloat = 170
        /// The disclosure scrolls past this height instead of growing the sheet.
        static let disclosureHeight: CGFloat = 180
        static let dotSize: CGFloat = 7
    }

    typealias C = SSHMachinesCopy

    let panel: SettingsPanel
    @ObservedObject private var model = SSHMachinesModel.shared

    @State private var user = ""
    @State private var host = ""
    @State private var portText = ""
    @State private var label = ""
    @State private var purge = false
    @State private var scopeOpen = true

    var body: some View {
        panel.infoSection(C.section, summary: C.summary, details: { panel.hint(C.details) }, accessory: {
            Button {
                if model.sheet == .add { model.cancelAdd() } else { openAdd() }
            } label: { Label(C.addNew.localized, systemImage: model.sheet == .add ? "minus" : "plus") }
                .controlSize(.small)
                .disabled(model.working || model.consent != nil || (model.sheet != nil && model.sheet != .add))
        }) {
            if model.rows.isEmpty { panel.hint(C.noMachines) }
            ForEach(model.rows) { machineRow($0) }
            if let sheet = model.sheet {
                switch sheet {
                case .add: addCard()
                case .consent: consentCard()
                case .remove(let id): removeCard(id)
                }
            } else if let notice = model.notice {
                noticeText(notice)
            }
        }
        .onAppear { model.pageAppeared() }
        .onChange(of: model.consent != nil) { _, asking in
            if asking { scopeOpen = true }
        }
        .onDisappear {
            if model.sheet != nil { model.sheetDismissed() }
            model.pageDisappeared()
        }
        .alert(
            C.identityTitle.localized,
            isPresented: Binding(
                get: { model.identityPrompt != nil }, set: { if !$0 { model.resolveIdentity(false) } }),
            presenting: model.identityPrompt
        ) { _ in
            Button(C.identityAccept.localized) { model.resolveIdentity(true) }
            Button(DevicesCopy.cancel.localized, role: .cancel) { model.resolveIdentity(false) }
        } message: { prompt in
            Text(verbatim: C.identityMessage.localized(prompt.known, prompt.now))
        }
    }

    private func openAdd() {
        user = ""
        host = ""
        portText = ""
        label = ""
        model.openAdd()
    }

    // MARK: Rows

    private func dotColor(_ row: SSHMachinesModel.Row) -> Color {
        switch SSHMachinesModel.health(row) {
        case .ok: .green
        case .warning: .orange
        case .error: .red
        case .idle: .secondary
        }
    }

    @ViewBuilder
    private func machineRow(_ row: SSHMachinesModel.Row) -> some View {
        let machine = row.machine
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Circle()
                    .fill(dotColor(row))
                    .frame(width: Layout.dotSize, height: Layout.dotSize)
                Text(verbatim: machine.label)
                    .font(.caption.weight(.medium))
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .layoutPriority(1)
                if machine.addressText != machine.label {
                    Text(verbatim: machine.addressText)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 4)
                Text(verbatim: SSHMachinesModel.statusLine(row))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Menu {
                    Button(C.reapprove.localized) { model.reapprove(id: machine.id) }
                    Divider()
                    Button(DevicesCopy.remove.localized, role: .destructive) { model.sheet = .remove(machine.id) }
                } label: {
                    Image(systemName: "ellipsis")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .disabled(model.working)
            }
            if row.needsReapproval {
                attention(C.needsReapproval.localized, action: C.reapprove) { model.reapprove(id: machine.id) }
            }
            if case .portInUse(let port) = row.tunnel { noticeText(C.portBusy.localized(port)) }
            if let command = SSHMachinesModel.hostKeyCommand(row) {
                noticeText(C.hostKey.localized(command))
            } else if case .backoff(_, _, let reason) = row.tunnel, !reason.isEmpty {
                Text(verbatim: reason)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .glassCard(cornerRadius: 8)
    }

    /// One line that needs the owner, and the button that answers it.
    private func attention(_ text: String, action: String, perform: @escaping () -> Void) -> some View {
        HStack(spacing: 8) {
            noticeText(text)
            Spacer(minLength: 4)
            Button(action.localized, action: perform)
                .controlSize(.small)
                .disabled(model.working)
        }
    }

    // MARK: Add

    /// The add form; while a flow waits for the owner's consent the same sheet
    /// shows that instead, so the two never stack on each other.
    @ViewBuilder
    private func addCard() -> some View {
        if let request = model.consent {
            consentContent(request)
        } else {
            addForm()
        }
    }

    @ViewBuilder
    private func addForm() -> some View {
        formCard {
            Text(C.addHeader.localized).font(.headline)
            if !model.canAdd { noticeText(C.noAgent.localized) }
            field(C.host, text: $host)
            field(C.user, text: $user)
            field(C.sshPort, text: $portText)
            field(C.label, text: $label)
            if let notice = model.notice { noticeText(notice) }
        } footer: {
            HStack(spacing: 8) {
                if model.working {
                    ProgressView().controlSize(.small)
                    Text(C.working.localized).font(.caption2).foregroundStyle(.secondary)
                }
                Spacer()
                Button(DevicesCopy.cancel.localized, role: .cancel) { model.cancelAdd() }
                    .keyboardShortcut(.cancelAction)
                Button(C.addDeploy.localized) {
                    model.add(user: user, host: host, portText: portText, label: label)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!model.canAdd || host.isEmpty || model.working)
            }
        }
    }

    private func field(_ title: String, text: Binding<String>) -> some View {
        panel.row(title) {
            TextField("", text: text)
                .textFieldStyle(.roundedBorder)
                .font(.caption)
                .frame(width: Layout.fieldWidth)
                .disabled(model.working)
        }
    }

    // MARK: Sheets

    @ViewBuilder
    private func consentCard() -> some View {
        if let request = model.consent { consentContent(request) }
    }

    private func consentContent(_ request: SSHConsentRequest) -> some View {
        formCard {
            Text(verbatim: (request.kind == .add ? C.consentTitle : C.reapproveTitle)
                .localized(request.target.destination))
                .font(.headline)
                .lineLimit(2)
            Text(C.consentSummary.localized)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if request.willTurnSharingOn { noticeText(C.turnsOn.localized) }
            baseline(request)
            if request.agent.change == .unchanged {
                Text(agentText(.unchanged)).font(.caption2).foregroundStyle(.secondary)
            } else {
                noticeText(agentText(request.agent.change))
            }
            if request.endsOtherGrants {
                noticeText(C.endsOthers.localized(request.existingGrantCount))
            }
            if !request.canApprove { noticeText(C.scopeTruncated.localized) }
            DisclosureGroup(C.scopeHeader.localized, isExpanded: $scopeOpen) {
                ScrollView {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(Array(request.disclosure.lines.enumerated()), id: \.offset) { _, line in
                            VStack(alignment: .leading, spacing: 0) {
                                Text(verbatim: line.name).font(.caption2.weight(.semibold))
                                Text(verbatim: line.value)
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                                    .textSelection(.enabled)
                            }
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(height: Layout.disclosureHeight)
                .padding(.top, 4)
            }
            .font(.caption)
        } footer: {
            HStack {
                Spacer()
                Button(DevicesCopy.cancel.localized, role: .cancel) { model.resolveConsent(false) }
                    .keyboardShortcut(.cancelAction)
                // No default action: the owner presses Approve on purpose, after reading the scope.
                Button(C.approvePair.localized) { model.resolveConsent(true) }
                    .disabled(!request.canApprove)
            }
        }
    }

    private func agentText(_ change: SSHAgentSetup.Change) -> String {
        switch change {
        case .installed: C.agentInstalled.localized
        case .replaced: C.agentReplaced.localized
        case .started: C.agentStarted.localized
        case .unchanged: C.agentUnchanged.localized
        }
    }

    @ViewBuilder
    private func baseline(_ request: SSHConsentRequest) -> some View {
        let status = request.baseline
        VStack(alignment: .leading, spacing: 2) {
            Text(C.baselineHeader.localized).font(.caption.weight(.semibold))
            Text((status.sharingEnabled ? C.sharingOn : C.sharingOff).localized)
            Text(C.grantCount.localized(status.activeGrantCount))
            if let host = status.bindHost {
                Text(verbatim: C.listening.localized(
                    host + (status.bindPort.map { ":\($0)" } ?? "")))
            } else {
                Text(C.notListening.localized)
            }
            Text((status.scopeApproved ? C.scopeAlready : C.scopeNotYet).localized)
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private func removeCard(_ id: UUID) -> some View {
        let name = model.rows.first { $0.id == id }?.machine.label ?? ""
        card {
            Text(verbatim: C.removeTitle.localized(name)).font(.headline).lineLimit(2)
            Text(C.removeMessage.localized)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Toggle(C.purge.localized, isOn: $purge)
                .font(.caption)
            HStack {
                Spacer()
                Button(DevicesCopy.cancel.localized, role: .cancel) { model.sheet = nil }
                    .keyboardShortcut(.cancelAction)
                Button(DevicesCopy.remove.localized, role: .destructive) {
                    let purge = purge
                    self.purge = false
                    model.sheet = nil
                    model.remove(id: id, purge: purge)
                }
                .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: Pieces

    /// An inline card in the section: content, then the action row. The page
    /// scrolls, so the card is never clipped and the buttons are always there.
    private func card<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 10) { content() }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .padding(.vertical, 10)
            .glassCard(cornerRadius: 8)
    }

    private func formCard<Content: View, Footer: View>(
        @ViewBuilder content: () -> Content,
        @ViewBuilder footer: () -> Footer
    ) -> some View {
        card {
            content()
            footer().fixedSize(horizontal: false, vertical: true)
        }
    }

    /// A state notice: the orange caption the rest of the page uses. The text is
    /// already localized (and may hold remote text), so it is shown as is.
    private func noticeText(_ text: String) -> some View {
        Text(verbatim: text)
            .font(.caption2)
            .foregroundStyle(.orange)
            .fixedSize(horizontal: false, vertical: true)
            .textSelection(.enabled)
    }
}
