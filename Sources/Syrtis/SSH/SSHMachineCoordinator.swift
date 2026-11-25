import Foundation
import TokenBarCore

/// The local side of pairing (`tb_remote_*`). Injected so tests need no agent.
protocol SSHLocalPairing: Sendable {
    func rehost(pairUrl: String, localPort: Int) throws -> String
    func claim(pairUrl: String) throws -> RemoteClaim
    /// Blocks until the producer approves (up to 960 s in the core).
    func poll(handle: Int64) throws -> RemotePaired
    func peerRemove(producerStatic: String) throws
    /// The local agent's own listener port, kept out of lport allocation.
    func agentListenPort() -> Int?
}

struct LiveSSHLocalPairing: SSHLocalPairing {
    private func wrap<T>(_ body: () throws -> T) throws -> T {
        do { return try body() } catch TBCoreError.bridge(let reason) {
            throw SSHRemoteError.pairing(RemoteAgentLaunch.isClassString(reason) ? reason : "error")
        }
    }

    func rehost(pairUrl: String, localPort: Int) throws -> String {
        try wrap { try TBCore.remotePairUrlRehost(pairUrl: pairUrl, localPort: localPort).pairUrl }
    }
    func claim(pairUrl: String) throws -> RemoteClaim { try wrap { try TBCore.remotePairClaim(pairUrl: pairUrl) } }
    func poll(handle: Int64) throws -> RemotePaired { try wrap { try TBCore.remotePairPoll(handle: handle) } }
    func peerRemove(producerStatic: String) throws {
        try wrap { try TBCore.remotePeerRemove(producerStatic: producerStatic) }
    }
    func agentListenPort() -> Int? { (try? TBCore.remoteStatus())?.bindPort }
}

/// What this flow did to the agent on the machine, against what was there.
struct SSHAgentSetup: Equatable, Sendable {
    enum Change: Equatable, Sendable {
        /// No agent was there; this flow installed and started one.
        case installed
        /// An agent was there; it was stopped, replaced and started.
        case replaced
        /// The agent was installed but not running; it was started.
        case started
        case unchanged
    }

    let before: SSHAgentBaseline
    /// The deploy script ran (a fresh install or a replacement).
    var deployed = false
    /// An installed agent that was not running was started (re-approval).
    var started = false

    init(before: SSHAgentBaseline = .missing) { self.before = before }

    /// Something on the machine is not as it was.
    var touched: Bool { deployed || started }

    var change: Change {
        if deployed { return before.installed ? .replaced : .installed }
        return started ? .started : .unchanged
    }
}

/// What the owner confirms before the scan scope is approved. `disclosure` is
/// shown with `Text(verbatim:)`.
struct SSHConsentRequest: Sendable {
    enum Kind: Sendable { case add, reapprove }

    let kind: Kind
    let target: SSHTarget
    /// `status` before the scope is approved: what a cancel returns the remote to.
    let baseline: SSHRemoteStatus
    let disclosure: SSHDisclosure
    /// What was done to the agent to get this far; a cancel undoes it.
    var agent = SSHAgentSetup()

    /// Approving a scope the owner has not approved ends other consumers'
    /// grants (they must re-approve); an already approved scope is left alone.
    var endsOtherGrants: Bool { !baseline.scopeApproved && baseline.activeGrantCount > 0 }
    var existingGrantCount: Int { baseline.activeGrantCount }
    var willTurnSharingOn: Bool { !baseline.sharingEnabled }
    /// The digest covers the whole scope; a list cut for display is not approved.
    var canApprove: Bool { !disclosure.truncated }
}

typealias SSHConsent = @Sendable (SSHConsentRequest) async -> Bool
/// Called before approval when the producer's key differs from the stored one:
/// the old and the new fingerprint. False cancels and rolls back.
typealias SSHIdentityConfirm = @Sendable (_ old: String, _ new: String) async -> Bool

struct SSHReapproveResult: Sendable {
    let machine: SSHMachine
    /// The producer's `responderStatic` changed and the owner accepted it.
    let identityChanged: Bool
}

struct SSHRemoveResult: Sendable {
    /// Steps on the machine that did not run (it could not be reached, the
    /// agent refused, or a purge left something), as lines to paste into a
    /// terminal. The machine is gone from this Mac either way.
    let manualCommands: [String]
    let error: SSHRemoteError?
    /// A purge was asked for but this many other devices still hold a grant
    /// there, so the agent was left installed.
    var purgeSkippedGrants: Int?
}

/// Add, re-approve and remove a machine (no UI). Every method blocks only on
/// helper threads; one flow runs at a time (`.busy` otherwise). A failed or
/// cancelled flow undoes what it did on the machine before it returns; what it
/// cannot undo (the machine is unreachable) comes back as the commands to run
/// by hand (`SSHRollbackIncomplete`). Nothing is saved or retried.
final class SSHMachineCoordinator: @unchecked Sendable {
    struct Dependencies: Sendable {
        var store: SSHMachines
        var tunnels: any SSHTunnelControlling
        var local: any SSHLocalPairing
        var makeRemote: @Sendable (SSHTarget, SSHCancellation?) -> any SSHRemoteControl
        var binaryFor: @Sendable (String) -> URL?
        var pin: String = SSHAgentPin.sha256
        var randomRemotePort: @Sendable () -> Int = { Int.random(in: SSHInput.portRange) }
        var tunnelTimeout: TimeInterval = 20
        var offerTTL = 120
        var claimAttempts = 5
    }

    private let deps: Dependencies
    private let gate = NSLock()
    private var running = false

    init(_ dependencies: Dependencies) { deps = dependencies }

    private func begin() throws {
        gate.lock()
        defer { gate.unlock() }
        guard !running else { throw SSHRemoteError.busy }
        running = true
    }

    private func end() {
        gate.lock()
        running = false
        gate.unlock()
    }

    private func io<T: Sendable>(_ body: @escaping @Sendable () throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(with: Result { try body() })
            }
        }
    }

    private func check(_ cancel: SSHCancellation) throws {
        if cancel.isCancelled { throw SSHRemoteError.cancelled }
    }

    /// What an add or re-approve has changed so far, so one rollback can put it
    /// back. The flow runs one step at a time; nothing here is shared.
    private final class Flow: @unchecked Sendable {
        let target: SSHTarget
        let machineId: UUID
        var agent = SSHAgentSetup()
        /// `status` before the scope was approved.
        var baseline: SSHRemoteStatus?
        var offerId: String?
        var grantId: String?
        var sharingOn = false
        /// An approval may have gone through without its id reaching this Mac.
        var grantUnknown = false
        var tunnelTouched = false
        /// The saved record before a re-approval (nil for an add): its forward is what a rollback restores.
        let original: SSHMachine?

        init(target: SSHTarget, machineId: UUID, original: SSHMachine? = nil) {
            self.target = target
            self.machineId = machineId
            self.original = original
        }
    }

    // MARK: Add

    /// Installs the bundled agent (always uploaded and checked against the
    /// pin), shows the baseline and the scan scope to `confirm`, and only after
    /// a yes approves, shares, listens and pairs. A no, a cancel or a failure
    /// undoes what this flow did: a new agent is uninstalled, one that was off
    /// is stopped again, one that was running is started again (a replaced
    /// build is not restored).
    func add(
        user: String? = nil, host: String, sshPort: Int? = nil, label: String?,
        confirm: @escaping SSHConsent, cancel: SSHCancellation = SSHCancellation()
    ) async throws -> SSHMachine {
        try begin()
        defer { end() }
        let target = try SSHTarget(user: user, host: host, port: sshPort)
        guard !deps.store.all().contains(where: {
            $0.user == user && $0.host == host && $0.sshPort == sshPort
        }) else { throw SSHRemoteError.invalidInput("duplicate machine") }
        let pin = deps.pin
        guard SSHInput.isDigest(pin), !SSHAgentPin.isPlaceholder(pin) else {
            throw SSHRemoteError.agentPinMissing
        }
        let remote = deps.makeRemote(target, cancel)
        let flow = Flow(target: target, machineId: UUID())
        do {
            return try await addSteps(
                flow: flow, remote: remote, target: target, user: user, host: host, sshPort: sshPort,
                label: label, pin: pin, confirm: confirm, cancel: cancel)
        } catch {
            throw await rollback(flow, after: error)
        }
    }

    private func addSteps(
        flow: Flow, remote: any SSHRemoteControl, target: SSHTarget, user: String?, host: String,
        sshPort: Int?, label: String?, pin: String, confirm: @escaping SSHConsent, cancel: SSHCancellation
    ) async throws -> SSHMachine {
        let before = try await io { try remote.probeAgent() }
        flow.agent = SSHAgentSetup(before: before)
        try check(cancel)
        let binaryFor = deps.binaryFor
        flow.agent.deployed = true
        try await io { try remote.deploy(binaryFor: binaryFor, pin: pin) }
        try check(cancel)
        let baseline = try await io { try remote.status() }
        flow.baseline = baseline
        let disclosure = try await io { try remote.disclosure() }
        try Self.checkListener(baseline)
        let request = SSHConsentRequest(
            kind: .add, target: target, baseline: baseline, disclosure: disclosure, agent: flow.agent)
        guard await confirm(request) else { throw SSHRemoteError.declined }
        try check(cancel)
        guard !disclosure.truncated else { throw SSHRemoteError.disclosureTruncated }

        let localPort = try deps.store.allocateLocalPort(
            avoiding: Set([deps.local.agentListenPort()].compactMap { $0 }))
        let draft = SSHMachine(
            id: flow.machineId, label: SSHInput.cleanedLabel(label ?? "") ?? target.destination,
            user: user, host: host, sshPort: sshPort, remotePort: deps.randomRemotePort(),
            localPort: localPort, producerStatic: nil, fingerprint: nil, grantId: nil, enabled: true)
        let store = deps.store
        let outcome = try await pair(
            flow: flow, remote: remote, baseline: baseline, disclosure: disclosure,
            machine: draft, expectedFingerprint: nil, confirmIdentity: nil, cancel: cancel
        ) { paired in
            var machine = draft
            machine.remotePort = paired.remotePort
            machine.producerStatic = paired.producerStatic
            machine.fingerprint = paired.fingerprint
            machine.grantId = paired.grantId
            try store.add(machine)
            return machine
        }
        return outcome.machine
    }

    // MARK: Re-approve

    /// A grant that ended (`SSHMachines.needsReapproval`): the same flow as
    /// `add` on the saved machine, keeping its local port and without
    /// installing anything (an agent that is off is started and stopped again on
    /// a cancel). A changed producer key goes to `confirmIdentityChange` before
    /// anything is approved.
    func reapprove(
        id: UUID, confirm: @escaping SSHConsent, confirmIdentityChange: @escaping SSHIdentityConfirm,
        cancel: SSHCancellation = SSHCancellation()
    ) async throws -> SSHReapproveResult {
        try begin()
        defer { end() }
        guard let original = deps.store.machine(id: id) else { throw SSHRemoteError.invalidInput("machine") }
        let target = try original.target()
        let remote = deps.makeRemote(target, cancel)
        let flow = Flow(target: target, machineId: id, original: original)
        do {
            return try await reapproveSteps(
                flow: flow, remote: remote, original: original, target: target,
                confirm: confirm, confirmIdentityChange: confirmIdentityChange, cancel: cancel)
        } catch {
            throw await rollback(flow, after: error)
        }
    }

    private func reapproveSteps(
        flow: Flow, remote: any SSHRemoteControl, original: SSHMachine, target: SSHTarget,
        confirm: @escaping SSHConsent, confirmIdentityChange: @escaping SSHIdentityConfirm,
        cancel: SSHCancellation
    ) async throws -> SSHReapproveResult {
        let before = try await io { try remote.probeAgent() }
        guard before.installed else { throw SSHRemoteError.agentMissing }
        flow.agent = SSHAgentSetup(before: before)
        if !before.running {
            flow.agent.started = true
            _ = try await io { try remote.ensureRunning() }
        }
        try check(cancel)
        let baseline = try await io { try remote.status() }
        flow.baseline = baseline
        let disclosure = try await io { try remote.disclosure() }
        try Self.checkListener(baseline)
        let request = SSHConsentRequest(
            kind: .reapprove, target: target, baseline: baseline, disclosure: disclosure, agent: flow.agent)
        guard await confirm(request) else { throw SSHRemoteError.declined }
        try check(cancel)
        guard !disclosure.truncated else { throw SSHRemoteError.disclosureTruncated }

        let store = deps.store, local = deps.local
        let outcome = try await pair(
            flow: flow, remote: remote, baseline: baseline, disclosure: disclosure,
            machine: original, expectedFingerprint: original.fingerprint,
            confirmIdentity: confirmIdentityChange, cancel: cancel
        ) { paired in
            var machine = original
            machine.remotePort = paired.remotePort
            machine.producerStatic = paired.producerStatic
            machine.fingerprint = paired.fingerprint
            machine.grantId = paired.grantId
            try store.update(machine)
            if let old = original.producerStatic, old != paired.producerStatic {
                try? local.peerRemove(producerStatic: old)
            }
            return machine
        }
        return SSHReapproveResult(
            machine: outcome.machine, identityChanged: outcome.machine.producerStatic != original.producerStatic)
    }

    // MARK: Remove

    /// Revokes this receiver's grant (and turns sharing off only when none is
    /// left), forgets the peer, stops the tunnel and deletes the machine. With
    /// `purge` the agent is uninstalled too, but only when no other device
    /// holds a grant there. When the machine cannot be reached the result
    /// carries the commands to run by hand; the machine is removed here anyway.
    func remove(id: UUID, purge: Bool = false) async throws -> SSHRemoveResult {
        try begin()
        defer { end() }
        guard let machine = deps.store.machine(id: id) else { throw SSHRemoteError.invalidInput("machine") }
        let outcome = await revokeRemote(machine, purge: purge)
        deps.tunnels.stop(id: id)
        if let key = machine.producerStatic {
            let local = deps.local
            _ = try? await io { try local.peerRemove(producerStatic: key) }
        }
        deps.store.remove(id: id)
        return outcome
    }

    private func revokeRemote(_ machine: SSHMachine, purge: Bool) async -> SSHRemoveResult {
        guard let target = try? machine.target() else { return SSHRemoveResult(manualCommands: [], error: nil) }
        let remote = deps.makeRemote(target, nil)
        let grantId = machine.grantId
        do {
            let skipped = try await io { () -> Int? in
                let probe = try remote.probeAgent()
                guard probe.installed else { return nil }
                var startedHere = false
                if !probe.running {
                    // Off: nothing is shared; it runs only long enough to revoke or to be removed.
                    guard grantId != nil || purge else { return nil }
                    do { _ = try remote.ensureRunning() } catch SSHRemoteError.agentMissing { return nil }
                    startedHere = true
                }
                if let grantId {
                    // 5: the agent does not know an active grant by that id.
                    do { try remote.revokeGrant(id: grantId) } catch SSHRemoteError.ctl(code: 5, _) {}
                }
                // Read after our own grant is gone: what is left belongs to others.
                let status = try remote.status()
                if status.sharingEnabled, status.activeGrantCount == 0 { try remote.setSharing(false) }
                var skipped: Int?
                if purge {
                    if status.activeGrantCount == 0 {
                        try? remote.shutdown()
                        do { try remote.uninstall(purge: true) } catch SSHRemoteError.ctl(code: 7, let detail) {
                            throw PurgeLeftover(detail: detail)
                        }
                        startedHere = false
                    } else {
                        skipped = status.activeGrantCount
                    }
                }
                if startedHere { try Self.stop(remote) }
                return skipped
            }
            return SSHRemoveResult(manualCommands: [], error: nil, purgeSkippedGrants: skipped)
        } catch let leftover as PurgeLeftover {
            // The agent is stopped and its grants are gone: rerunning the purge is all that is left.
            return SSHRemoveResult(
                manualCommands: SSHManualCommands.lines([.uninstall(purge: true)], on: target),
                error: .ctl(code: 7, detail: leftover.detail))
        } catch {
            // Purge ends other devices' access too: it follows only a status that shows no grants.
            var lines = SSHManualCommands.lines((grantId.map { [.grantRevoke(grantId: $0)] } ?? []) + (purge ? [.status] : []), on: target)
            if purge {
                lines.append("# only if activeGrantCount is 0:")
                lines += SSHManualCommands.lines([.shutdown, .uninstall(purge: true)], on: target)
            }
            return SSHRemoveResult(
                manualCommands: lines,
                error: (error as? SSHRemoteError) ?? .unreachable(""))
        }
    }

    /// `uninstall --purge` exited 7: it left something, or a safety check stopped it.
    private struct PurgeLeftover: Error { let detail: String }

    // MARK: Pairing

    private static func checkListener(_ baseline: SSHRemoteStatus) throws {
        if let host = baseline.bindHost, host != "127.0.0.1" { throw SSHRemoteError.listenerNotLoopback }
    }

    private struct Paired: Sendable {
        let producerStatic: String
        let fingerprint: String
        let grantId: String
        let remotePort: Int
    }

    private struct Outcome: Sendable { let machine: SSHMachine }

    /// After a changing call (`pending approve`, `sharing on`) failed: the agent
    /// may have applied it before the answer was lost. `ctl` 6 (no valid reply)
    /// and 7 (may have taken effect) say so too; any other `ctl` exit is a refusal.
    private static func mayHaveApplied(_ error: Error) -> Bool {
        guard let error = error as? SSHRemoteError else { return true }
        switch error {
        case .timedOut, .cancelled, .outputLimit, .malformedOutput, .unreachable: return true
        case .ctl(code: 6, _), .ctl(code: 7, _): return true
        default: return false
        }
    }

    /// Approve, share, listen, offer, tunnel, claim, approve the SAS, poll,
    /// then `commit`. A failure ends the local claim and the peer it added (never
    /// one a saved machine holds, so a re-approval keeps its original) here and
    /// leaves the remote to `rollback`.
    private func pair(
        flow: Flow, remote: any SSHRemoteControl, baseline: SSHRemoteStatus,
        disclosure: SSHDisclosure, machine start: SSHMachine, expectedFingerprint: String?,
        confirmIdentity: SSHIdentityConfirm?, cancel: SSHCancellation,
        commit: @escaping @Sendable (Paired) throws -> SSHMachine
    ) async throws -> Outcome {
        var machine = start
        var claimHandle: Int64?
        var pairedKey: String?
        let local = deps.local
        do {
            // A re-approval confirms a changed identity first: approving a scope ends other devices' grants.
            let scopeLater = expectedFingerprint != nil
            if !baseline.scopeApproved, !scopeLater {
                try await io { try remote.approveScope(digest: disclosure.digest) }
            }
            if !baseline.sharingEnabled {
                do {
                    try await io { try remote.setSharing(true) }
                    flow.sharingOn = true
                } catch {
                    // A refusal changed nothing; sharing that someone else turned on is not ours to turn off.
                    if Self.mayHaveApplied(error) { flow.sharingOn = true }
                    throw error
                }
            }
            try check(cancel)

            // An existing loopback listener is reused as it is; otherwise listen
            // on a loopback port chosen here (the baseline has no host).
            var fixedRemotePort = false
            var tried: Set<Int> = []
            if baseline.bindHost == "127.0.0.1", let port = baseline.bindPort, SSHInput.isPort(port) {
                machine.remotePort = port
                fixedRemotePort = true
            } else {
                if !SSHInput.portRange.contains(machine.remotePort) { machine.remotePort = deps.randomRemotePort() }
                let port = machine.remotePort
                try await io { try remote.listen(host: "127.0.0.1", port: port) }
            }
            tried.insert(machine.remotePort)

            var claim: RemoteClaim?
            var lastError: Error = SSHRemoteError.pairing("claim")
            for attempt in 0..<(fixedRemotePort ? 1 : max(deps.claimAttempts, 1)) {
                try check(cancel)
                if attempt > 0 {
                    // The previous port could not be claimed through: another one.
                    if let old = flow.offerId { _ = try? await io { try remote.cancelOffer(id: old) } }
                    flow.offerId = nil
                    var next = deps.randomRemotePort()
                    var guardCount = 0
                    while tried.contains(next), guardCount < 200 { next = deps.randomRemotePort(); guardCount += 1 }
                    let chosen = next
                    machine.remotePort = chosen
                    tried.insert(chosen)
                    try await io { try remote.listen(host: "127.0.0.1", port: chosen) }
                }
                let ttl = deps.offerTTL
                let offer = try await io { try remote.createOffer(ttl: ttl) }
                flow.offerId = offer.offerId
                flow.tunnelTouched = true
                deps.tunnels.start(machine)
                switch deps.tunnels.waitUntilUp(id: machine.id, timeout: deps.tunnelTimeout) {
                case .up: break
                case .portInUse(let port): throw SSHRemoteError.portInUse(port)
                case .backoff(_, _, let reason): throw SSHRemoteError.tunnelNotUp(reason)
                default: throw SSHRemoteError.tunnelNotUp("")
                }
                let port = machine.localPort
                do {
                    claim = try await io {
                        let url = try local.rehost(pairUrl: offer.pairUrl, localPort: port)
                        return try local.claim(pairUrl: url)
                    }
                    break
                } catch {
                    lastError = error
                }
            }
            guard let claim else { throw lastError }
            claimHandle = claim.handle
            guard SSHInput.isSAS(claim.sas) else { throw SSHRemoteError.pairing("sas") }
            if let expected = expectedFingerprint, claim.producerFingerprint != expected {
                guard let confirmIdentity,
                      await confirmIdentity(expected, claim.producerFingerprint)
                else { throw SSHRemoteError.identityChanged }
            }
            try check(cancel)
            if !baseline.scopeApproved, scopeLater {
                try await io { try remote.approveScope(digest: disclosure.digest) }
            }
            let sas = claim.sas
            let pendingId: String
            do {
                pendingId = try await io { try remote.approvePending(sas: sas) }
            } catch {
                // The id comes only with the answer; without it the grant cannot be named.
                if Self.mayHaveApplied(error) { flow.grantUnknown = true }
                throw error
            }
            guard let grantId = SSHGrantId.derive(pendingId: pendingId) else {
                flow.grantUnknown = true
                throw SSHRemoteError.malformedOutput
            }
            flow.grantId = grantId
            let handle = claim.handle
            let paired = try await io { try local.poll(handle: handle) }
            pairedKey = paired.producerStatic
            try check(cancel)
            let result = try commit(
                Paired(
                    producerStatic: paired.producerStatic, fingerprint: paired.fingerprint,
                    grantId: grantId, remotePort: machine.remotePort))
            return Outcome(machine: result)
        } catch {
            let saved = Set(deps.store.all().compactMap(\.producerStatic))
            if let handle = claimHandle, pairedKey == nil { abandon(handle, keeping: saved) }
            if let key = pairedKey, !saved.contains(key) { _ = try? await io { try local.peerRemove(producerStatic: key) } }
            throw error
        }
    }

    /// A claim that is no longer wanted: its poll runs on to the end; if it then
    /// succeeds the peer is removed at once (as `DevicesModel.cancelWait`),
    /// unless a saved machine holds it.
    private func abandon(_ handle: Int64, keeping saved: Set<String>) {
        let local = deps.local
        DispatchQueue.global(qos: .utility).async {
            if let paired = try? local.poll(handle: handle), !saved.contains(paired.producerStatic) {
                try? local.peerRemove(producerStatic: paired.producerStatic)
            }
        }
    }

    // MARK: Rollback

    /// Puts the remote back as the flow found it, touching only what this flow
    /// did: cancel its offer, revoke its own grant, `sharing off` only if the
    /// baseline had sharing off and no other grant is held, and the agent (uninstall a new one, stop one
    /// that was off, start a replaced one again). `ctl` cannot clear a listen
    /// address, so a loopback listener chosen here stays configured (sharing off
    /// stops it). A step that fails is not retried: the flow's error comes back
    /// wrapped with the commands to run by hand, and after a lost connection the
    /// remaining steps are listed without trying them.
    private func rollback(_ flow: Flow, after error: Error) async -> Error {
        if flow.tunnelTouched {
            // A re-approval may have moved the forward to another remote port: the saved one comes back.
            if let original = flow.original, original.enabled { deps.tunnels.start(original) }
            else { deps.tunnels.stop(id: flow.machineId) }
        }
        let remote = deps.makeRemote(flow.target, nil)
        let sharingWasOff = flow.baseline.map { !$0.sharingEnabled } ?? false
        var owed: [SSHCtlCommand] = []
        var unreachable = false
        /// Runs one undo step (true: nothing is left); a step that fails is owed by hand.
        func step(_ owes: [SSHCtlCommand], _ body: @escaping @Sendable () throws -> Bool) async {
            if unreachable { owed += owes; return }
            do {
                let done = try await io(body)
                if !done { owed += owes }
            } catch {
                owed += owes
                if (error as? SSHRemoteError)?.isConnectionFailure == true { unreachable = true }
            }
        }
        // An unreached cancel leaves an offer that expires by itself.
        if let offer = flow.offerId { await step([]) { try remote.cancelOffer(id: offer); return true } }
        if let grant = flow.grantId {
            await step([.grantRevoke(grantId: grant)]) { try remote.revokeGrant(id: grant); return true }
        }
        if flow.sharingOn, sharingWasOff {
            // `sharing off` ends every grant. A grant beyond what the baseline had is another consumer's, so
            // it is left to the owner; with an unnamed grant of ours possible the count cannot tell whose.
            let allowed = flow.grantUnknown ? Int.max : (flow.baseline.map { $0.scopeApproved ? $0.activeGrantCount : 0 } ?? 0)
            await step([.status, .sharing(false)]) {
                let status = try remote.status()
                guard status.sharingEnabled else { return true }
                guard status.activeGrantCount <= allowed else { return false }
                try remote.setSharing(false)
                return true
            }
        } else if flow.grantUnknown, let baseline = flow.baseline {
            // The approval may have gone through unnamed. Our own approval of the
            // scope ended every earlier grant; more than that is an orphan that
            // only the owner can end (`sharing off` ends everyone's).
            let expected = baseline.scopeApproved ? baseline.activeGrantCount : 0
            await step([.status, .sharing(false)]) { try remote.status().activeGrantCount <= expected }
        }
        if flow.agent.touched {
            let agent = flow.agent
            await step(Self.agentOwed(agent.before)) { try Self.restoreAgent(remote, agent); return true }
        }
        guard !owed.isEmpty else { return error }
        return SSHRollbackIncomplete(cause: error, commands: SSHManualCommands.lines(owed, on: flow.target))
    }

    /// What undoing the agent change takes by hand: remove a new agent, stop
    /// one that was off. A running one stays as it is.
    private static func agentOwed(_ before: SSHAgentBaseline) -> [SSHCtlCommand] {
        if !before.installed { return [.shutdown, .uninstall(purge: false)] }
        return before.running ? [] : [.shutdown]
    }

    /// Shutdown that fails when the answer is lost (so the stop stays owed);
    /// an agent that is already off (`ctl` code 4) is what was wanted.
    private static func stop(_ remote: any SSHRemoteControl) throws {
        do { try remote.shutdown() } catch SSHRemoteError.ctl(code: 4, _) {}
    }

    /// An agent this flow installed goes (no purge) unless another device
    /// holds a grant there; one that was off is stopped again; one that was
    /// running is made sure to run (the deploy stopped it).
    private static func restoreAgent(_ remote: any SSHRemoteControl, _ setup: SSHAgentSetup) throws {
        if !setup.before.installed {
            let count: Int
            do { count = try remote.status().activeGrantCount } catch SSHRemoteError.ctl(code: 4, _) {
                count = 0
            } catch SSHRemoteError.agentMissing { return }
            guard count == 0 else { return }
            try? remote.shutdown()
            try remote.uninstall(purge: false)
        } else if !setup.before.running {
            try stop(remote)
        } else {
            _ = try remote.ensureRunning()
        }
    }
}
