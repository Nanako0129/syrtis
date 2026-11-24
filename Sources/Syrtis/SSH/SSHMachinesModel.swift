import Foundation
import TokenBarCore

/// The state behind the Devices page's "SSH machines" section. Every action
/// goes through `SSHMachineCoordinator`; this holds what the page shows (rows,
/// notices) and the two confirmations a flow waits on. A process-wide singleton
/// like `DevicesModel`: Settings rebuilds the page when it switches pages.
@MainActor
final class SSHMachinesModel: ObservableObject {
    static let shared = SSHMachinesModel(runtime: .shared)

    struct Row: Identifiable, Equatable {
        let machine: SSHMachine
        let tunnel: SSHTunnelStatus
        /// The producer's grant ended (peer missing or not active here).
        let needsReapproval: Bool
        var id: UUID { machine.id }
    }

    /// What a row's state dot says: all fine, wants attention, broken, or just off.
    enum Health: Equatable { case ok, warning, error, idle }

    enum Sheet: Identifiable, Equatable {
        /// The add form. Its consent step is drawn inside this same sheet.
        case add
        /// A re-approval's consent (no add form behind it).
        case consent
        case remove(UUID)

        var id: String {
            switch self {
            case .add: "add"
            case .consent: "consent"
            case .remove(let id): "remove-\(id)"
            }
        }
    }

    struct IdentityPrompt: Equatable {
        let known: String
        let now: String
    }

    @Published private(set) var rows: [Row] = []
    /// A flow is running (deploying, waiting for a confirmation, pairing).
    @Published private(set) var working = false
    /// Already localized; may hold remote text, so it is shown verbatim.
    @Published private(set) var notice: String?
    @Published private(set) var consent: SSHConsentRequest?
    @Published var identityPrompt: IdentityPrompt?
    @Published var sheet: Sheet?

    private let runtime: SSHRemoteRuntime
    private let peersProvider: @Sendable () throws -> [RemotePeer]
    private let hasAgent: () -> Bool
    private let notifications: NotificationCenter
    private var pollTask: Task<Void, Never>?
    private var cancellation: SSHCancellation?
    private var consentContinuation: CheckedContinuation<Bool, Never>?
    private var identityContinuation: CheckedContinuation<Bool, Never>?

    init(
        runtime: SSHRemoteRuntime,
        peersProvider: @escaping @Sendable () throws -> [RemotePeer] = { try TBCore.remotePeers() },
        hasAgent: @escaping () -> Bool = { SSHAgentPin.isBundled() },
        notifications: NotificationCenter = .default
    ) {
        self.runtime = runtime
        self.peersProvider = peersProvider
        self.hasAgent = hasAgent
        self.notifications = notifications
    }

    /// False when this build carries no agent binary: adding is refused with
    /// a fixed line before any ssh call.
    var canAdd: Bool { hasAgent() }

    // MARK: Page lifetime

    func pageAppeared() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    /// A confirmation nobody can answer any more is a "no": the flow rolls
    /// back and the coordinator is free again.
    func pageDisappeared() {
        pollTask?.cancel()
        pollTask = nil
        resolveConsent(false)
        resolveIdentity(false)
        if sheet == .add { sheet = nil }
    }

    func refresh() async {
        let provider = peersProvider
        let peers = await Task.detached { try? provider() }.value
        let tunnels = runtime.supervisor
        rows = runtime.store.all().map { machine in
            Row(
                machine: machine, tunnel: tunnels.status(id: machine.id),
                // No answer from the local agent is not "no grant": judged only on a real list.
                needsReapproval: peers.map { SSHMachines.needsReapproval(machine, peers: $0) } ?? false)
        }
    }

    // MARK: Flows

    func add(user: String, host: String, portText: String, label: String) {
        guard !working else { return }
        guard hasAgent() else { notice = SSHMachinesCopy.noAgent.localized; return }
        let trimmedPort = portText.trimmingCharacters(in: .whitespaces)
        let typedPort = Int(trimmedPort)
        if !trimmedPort.isEmpty, !(typedPort.map { (1...65535).contains($0) } ?? false) {
            notice = SSHMachinesCopy.errInvalid.localized("SSH port")
            return
        }
        let port = typedPort
        let typedUser = user.trimmingCharacters(in: .whitespaces)
        let user = typedUser.isEmpty ? nil : typedUser
        let host = host.trimmingCharacters(in: .whitespaces)
        run(target: (user, host, port)) { [runtime] cancel in
            _ = try await runtime.coordinator.add(
                user: user, host: host, sshPort: port, label: label,
                confirm: { [weak self] in await self?.ask($0) ?? false }, cancel: cancel)
            return nil
        }
    }

    func openAdd() {
        guard !working else { return }
        notice = nil
        sheet = .add
    }

    /// The add sheet's Cancel: stops a running flow (its rollback runs, and the
    /// sheet stays to show the outcome); otherwise closes the sheet.
    func cancelAdd() {
        if working {
            cancelFlow()
        } else {
            notice = nil
            sheet = nil
        }
    }

    func reapprove(id: UUID) {
        guard !working, let machine = runtime.store.machine(id: id) else { return }
        run(target: (machine.user, machine.host, machine.sshPort)) { [runtime] cancel in
            let result = try await runtime.coordinator.reapprove(
                id: id,
                confirm: { [weak self] in await self?.ask($0) ?? false },
                confirmIdentityChange: { [weak self] old, new in await self?.askIdentity(old, new) ?? false },
                cancel: cancel)
            return result.identityChanged ? SSHMachinesCopy.identityKept.localized : nil
        }
    }

    func remove(id: UUID, purge: Bool) {
        guard !working else { return }
        working = true
        notice = nil
        Task {
            defer { working = false }
            do {
                notice = Self.removeNotice(try await runtime.coordinator.remove(id: id, purge: purge))
            } catch {
                notice = Self.message(for: error, command: nil)
            }
            await refresh()
            notifications.post(name: DevicesModel.peersChanged, object: nil)
        }
    }

    /// Cancels the running flow; its rollback runs as for any failure.
    func cancelFlow() {
        cancellation?.cancel()
        resolveConsent(false)
        resolveIdentity(false)
    }

    private func run(
        target: (user: String?, host: String, port: Int?),
        _ body: @escaping @Sendable (SSHCancellation) async throws -> String?
    ) {
        working = true
        notice = nil
        let cancel = SSHCancellation()
        cancellation = cancel
        let command = Self.sshCommand(user: target.user, host: target.host, port: target.port)
        Task {
            defer {
                working = false
                cancellation = nil
            }
            do {
                notice = try await body(cancel)
                if notice == nil, sheet == .add { sheet = nil }
            } catch {
                notice = Self.message(for: error, command: command)
            }
            await refresh()
            notifications.post(name: DevicesModel.peersChanged, object: nil)
        }
    }

    // MARK: Confirmations

    private func ask(_ request: SSHConsentRequest) async -> Bool {
        await withCheckedContinuation { continuation in
            consentContinuation = continuation
            consent = request
            if sheet != .add { sheet = .consent }
        }
    }

    /// The consent sheet's answer, or its dismissal (a no).
    func resolveConsent(_ approved: Bool) {
        let continuation = consentContinuation
        consentContinuation = nil
        consent = nil
        if sheet == .consent { sheet = nil }
        continuation?.resume(returning: approved)
    }

    private func askIdentity(_ old: String, _ new: String) async -> Bool {
        await withCheckedContinuation { continuation in
            identityContinuation = continuation
            identityPrompt = IdentityPrompt(known: old, now: new)
        }
    }

    func resolveIdentity(_ accepted: Bool) {
        let continuation = identityContinuation
        identityContinuation = nil
        identityPrompt = nil
        continuation?.resume(returning: accepted)
    }

    func sheetDismissed() {
        switch sheet {
        case .consent: resolveConsent(false)
        case .add: cancelAdd()
        default: sheet = nil
        }
    }

    // MARK: Text

    /// The command that confirms a host key by hand.
    nonisolated static func sshCommand(user: String?, host: String, port: Int?) -> String {
        let destination = user.map { "\($0)@\(host)" } ?? host
        guard let port, port != 22 else { return "ssh \(destination)" }
        return "ssh -p \(port) \(destination)"
    }

    /// What a finished remove shows: its error (with the agent's detail), then
    /// the commands left to run by hand; or the skipped purge; or nothing.
    nonisolated static func removeNotice(_ result: SSHRemoveResult) -> String? {
        let error = result.error.map { message(for: $0, command: nil) }
        if !result.manualCommands.isEmpty {
            let manual = SSHMachinesCopy.manualNotice.localized(result.manualCommands.joined(separator: "\n"))
            return error.map { $0 + "\n" + manual } ?? manual
        }
        if let error { return error }
        return result.purgeSkippedGrants.map { SSHMachinesCopy.purgeSkipped.localized($0) }
    }

    /// Fixed copy; remote text (stderr first line, a ctl detail) only as an argument.
    nonisolated static func message(for error: Error, command: String?) -> String {
        typealias C = SSHMachinesCopy
        if let incomplete = error as? SSHRollbackIncomplete {
            return message(for: incomplete.cause, command: command) + "\n"
                + C.manualNotice.localized(incomplete.commands.joined(separator: "\n"))
        }
        guard let error = error as? SSHRemoteError else { return C.errOther.localized }
        switch error {
        case .invalidInput(let field): return C.errInvalid.localized(field)
        case .launchFailed: return C.errLaunch.localized
        case .hostKeyRejected: return C.hostKey.localized(command ?? "ssh")
        case .unreachable(let line): return line.isEmpty ? C.errUnreachableBare.localized : C.errUnreachable.localized(line)
        case .timedOut: return C.errTimeout.localized
        case .outputLimit: return C.errOutput.localized
        case .cancelled: return C.errCancelled.localized
        case .malformedOutput: return C.errMalformed.localized
        case .unsupportedPlatform: return C.errPlatform.localized
        case .agentPinMissing: return C.noAgent.localized
        case .integrity: return C.errIntegrity.localized
        case .agentMissing: return C.errAgentMissing.localized
        case .agentCannotRun(let detail): return C.errCannotRun.localized(detail)
        case .ctl(let code, let detail): return C.errCommand.localized(Int(code), detail)
        case .unsafeRemoteDirs(let dirs):
            let fix = "\(command ?? "ssh") 'chmod go-w \(dirs.joined(separator: " "))'"
            return C.errUnsafeDirs.localized(dirs.joined(separator: ", "), fix)
        case .busy: return C.errBusy.localized
        case .portInUse(let port): return C.portBusy.localized(port)
        case .noFreePort: return C.errNoPort.localized
        case .declined: return C.errDeclined.localized
        case .listenerNotLoopback: return C.errNotLoopback.localized
        case .tunnelNotUp(let detail): return detail.isEmpty ? C.errTunnelBare.localized : C.errTunnel.localized(detail)
        case .pairing(let reason): return C.errPairing.localized(reason)
        case .identityChanged: return C.errIdentity.localized
        case .disclosureTruncated: return C.errTruncated.localized
        }
    }

    /// The one-line state of a row's forward; remote text only as an argument.
    static func statusLine(_ row: Row) -> String {
        typealias C = SSHMachinesCopy
        switch row.tunnel {
        case .up: return C.forwarding.localized
        case .starting: return C.connecting.localized
        case .stopped: return C.stopped.localized
        case .portInUse: return C.portInUse.localized
        case .backoff(_, let retryIn, _): return C.retrying.localized(Int(retryIn.rounded(.up)))
        }
    }

    /// The last ssh line of a backoff, when it says the host key is the cause.
    nonisolated static func hostKeyCommand(_ row: Row) -> String? {
        guard case .backoff(_, _, let reason) = row.tunnel, SSHInput.saysHostKeyRejected(reason) else { return nil }
        return sshCommand(user: row.machine.user, host: row.machine.host, port: row.machine.sshPort)
    }

    /// The state dot: a host key problem or a busy port is an error, a lost
    /// grant or a forward that is coming back wants attention.
    nonisolated static func health(_ row: Row) -> Health {
        if case .portInUse = row.tunnel { return .error }
        if hostKeyCommand(row) != nil { return .error }
        if row.needsReapproval { return .warning }
        switch row.tunnel {
        case .up: return .ok
        case .starting, .backoff: return .warning
        case .stopped: return .idle
        case .portInUse: return .error
        }
    }
}
