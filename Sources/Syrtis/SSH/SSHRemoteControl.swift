import Foundation
import TokenBarCore

/// What a flow needs from the remote agent. `SSHCtl` is the real one (over
/// `SSHRunner`); every method blocks.
protocol SSHRemoteControl: Sendable {
    func status() throws -> SSHRemoteStatus
    /// Read-only: whether the agent is installed and whether it answers.
    func probeAgent() throws -> SSHAgentBaseline
    func disclosure() throws -> SSHDisclosure
    func approveScope(digest: String) throws
    func setSharing(_ on: Bool) throws
    func listen(host: String, port: Int) throws
    func createOffer(ttl: Int) throws -> RemoteOffer
    func cancelOffer(id: String) throws
    /// The `pendingId` of the approved pairing.
    func approvePending(sas: String) throws -> String
    func revokeGrant(id: String) throws
    func shutdown() throws
    func uninstall(purge: Bool) throws
    /// Always uploads, verifies the sha256, installs and starts the agent, then
    /// waits until it answers `status`. A running agent is shut down first.
    func deploy(binaryFor: @Sendable (String) -> URL?, pin: String) throws
    /// Starts an installed agent that is not running and waits for `status`.
    func ensureRunning() throws -> SSHRemoteStatus
}

struct SSHCtl: SSHRemoteControl {
    let runner: SSHRunner
    let target: SSHTarget
    var cancel: SSHCancellation?
    var callTimeout: TimeInterval = 30
    var deployTimeout: TimeInterval = 300
    /// Waits for a started agent: attempts and the gap between them.
    var startAttempts = 15
    var startGap: TimeInterval = 1

    init(runner: SSHRunner = SSHRunner(), target: SSHTarget, cancel: SSHCancellation? = nil) {
        self.runner = runner
        self.target = target
        self.cancel = cancel
    }

    // MARK: Plumbing

    /// stdout of a command that exited 0; every other end is an error.
    private func stdout(
        of command: SSHRemoteCommand, stdin: SSHStdin = .none, timeout: TimeInterval? = nil,
        maxStdout: Int = 256 * 1024
    ) throws -> Data {
        let result = try runner.run(
            target, command, stdin: stdin, timeout: timeout ?? callTimeout, maxStdout: maxStdout,
            cancel: cancel)
        switch result.end {
        case .timedOut: throw SSHRemoteError.timedOut
        case .outputLimit: throw SSHRemoteError.outputLimit
        case .cancelled: throw SSHRemoteError.cancelled
        case .exited(0): return result.stdout
        case .exited(255):
            if SSHInput.saysHostKeyRejected(result.stderr) { throw SSHRemoteError.hostKeyRejected }
            let line = SSHInput.firstLine(result.stderr)
            sshLog.error("ssh failed: \(line, privacy: .private)")
            throw SSHRemoteError.unreachable(line)
        case .exited(127): throw SSHRemoteError.agentMissing
        case .exited(let code):
            let line = SSHInput.firstLine(result.stderr)
            sshLog.error("remote exit \(code): \(line, privacy: .private)")
            throw SSHRemoteError.ctl(code: code, detail: line)
        }
    }

    private func call<T: Decodable>(_ command: SSHCtlCommand, as type: T.Type) throws -> T {
        try SSHFrame.decode(type, from: stdout(of: command.remoteCommand()))
    }

    private struct Nothing: Decodable {}

    // MARK: Commands

    func status() throws -> SSHRemoteStatus { try call(.status, as: SSHRemoteStatus.self) }

    func probeAgent() throws -> SSHAgentBaseline {
        do {
            _ = try status()
            return SSHAgentBaseline(installed: true, running: true)
        } catch SSHRemoteError.ctl(code: 4, _) {
            return SSHAgentBaseline(installed: true, running: false)
        } catch SSHRemoteError.agentMissing {
            return .missing
        } catch SSHRemoteError.ctl {
            // Any other answer (an older agent without `ctl` exits 2) means it is
            // installed; deploying replaces it.
            return SSHAgentBaseline(installed: true, running: true)
        }
    }

    func disclosure() throws -> SSHDisclosure {
        try SSHDisclosure(decoding: stdout(of: SSHCtlCommand.scopeDisclosure.remoteCommand()))
    }

    func approveScope(digest: String) throws {
        _ = try call(.scopeApprove(digest: digest), as: Nothing.self)
    }

    func setSharing(_ on: Bool) throws { _ = try call(.sharing(on), as: Nothing.self) }

    func listen(host: String, port: Int) throws {
        _ = try call(.listen(host: host, port: port), as: Nothing.self)
    }

    func createOffer(ttl: Int) throws -> RemoteOffer {
        let offer = try call(.offerCreate(ttl: ttl), as: RemoteOffer.self)
        guard SSHInput.isId32(offer.offerId), offer.pairUrl.hasPrefix("syrtis://"),
              offer.pairUrl.utf8.count <= 4096
        else { throw SSHRemoteError.malformedOutput }
        return offer
    }

    func cancelOffer(id: String) throws { _ = try call(.offerCancel(offerId: id), as: Nothing.self) }

    private struct Approved: Decodable {
        let approved: Bool
        let pendingId: String
    }

    func approvePending(sas: String) throws -> String {
        let reply = try call(.pendingApprove(sas: sas), as: Approved.self)
        guard reply.approved, SSHInput.isId32(reply.pendingId) else {
            throw SSHRemoteError.malformedOutput
        }
        return reply.pendingId
    }

    func revokeGrant(id: String) throws { _ = try call(.grantRevoke(grantId: id), as: Nothing.self) }

    func shutdown() throws { _ = try call(.shutdown, as: Nothing.self) }

    func uninstall(purge: Bool) throws {
        // After a shutdown the agent needs a moment to exit; 9 means "retry".
        for attempt in 0..<10 {
            do {
                _ = try call(.uninstall(purge: purge), as: Nothing.self)
                return
            } catch SSHRemoteError.ctl(code: 9, _) where attempt < 9 {
                Thread.sleep(forTimeInterval: startGap)
            }
        }
    }

    // MARK: Deploy

    func platformTriple() throws -> String {
        try SSHPlatform.triple(fromUname: stdout(of: SSHRemoteCommand.uname, maxStdout: 256))
    }

    /// One read-only ssh call, before anything is uploaded or changed.
    func checkRemoteDirs() throws {
        let known = Set(SSHRemoteCommand.agentDirSuffixes.map { "~" + $0 })
        let out = try stdout(of: SSHRemoteCommand.permissionCheck, maxStdout: 1024)
        let lines = String(decoding: out, as: UTF8.self).split(separator: "\n").map(String.init)
        guard lines.allSatisfy(known.contains) else { throw SSHRemoteError.malformedOutput }
        if !lines.isEmpty { throw SSHRemoteError.unsafeRemoteDirs(lines) }
    }

    func deploy(binaryFor: @Sendable (String) -> URL?, pin: String) throws {
        guard SSHInput.isDigest(pin), !SSHAgentPin.isPlaceholder(pin) else {
            throw SSHRemoteError.agentPinMissing
        }
        try checkRemoteDirs()
        let triple = try platformTriple()
        guard let binary = binaryFor(triple) else { throw SSHRemoteError.unsupportedPlatform }
        // Refuse a swapped local file before any byte leaves the Mac.
        guard (try? SSHAgentPin.sha256(of: binary)) == pin else { throw SSHRemoteError.integrity }
        do {
            let out = try stdout(
                of: SSHRemoteCommand.deploy(pin: pin), stdin: .file(binary), timeout: deployTimeout,
                maxStdout: 4 * 1024)
            struct Installed: Decodable { let installed: Bool }
            guard try SSHFrame.decode(Installed.self, from: out).installed else {
                throw SSHRemoteError.malformedOutput
            }
        } catch SSHRemoteError.ctl(code: 72, _) {
            throw SSHRemoteError.integrity
        } catch SSHRemoteError.ctl(code: 73, let detail) {
            throw SSHRemoteError.agentCannotRun(detail)
        }
        _ = try waitForAgent()
    }

    // MARK: Price table

    /// The remote sha256 of each price file that exists. nil when the output is anything but
    /// lines of exactly `<64 hex>  <fixed name>` (unknown: compare nothing). Exit 74 and 75 come
    /// back as `.ctl` like any other exit.
    func pricingHashes() throws -> [SSHPricingFile: String]? {
        let out = try stdout(of: SSHRemoteCommand.pricingHashes, maxStdout: 512)
        var hashes: [SSHPricingFile: String] = [:]
        for line in String(decoding: out, as: UTF8.self).split(separator: "\n", omittingEmptySubsequences: false).dropLast() {
            let parts = line.split(separator: " ", omittingEmptySubsequences: false)
            guard parts.count == 3, parts[1].isEmpty, SSHInput.isDigest(String(parts[0])),
                  let file = SSHPricingFile(rawValue: String(parts[2])), hashes[file] == nil
            else { return nil }
            hashes[file] = String(parts[0])
        }
        guard out.isEmpty || out.last == 0x0A else { return nil }
        return hashes
    }

    /// Sends `data` as `file`; the remote checks it against `digest` before the rename.
    func uploadPricing(_ file: SSHPricingFile, data: Data, digest: String) throws {
        _ = try stdout(of: SSHRemoteCommand.pricingUpload(file, digest: digest), stdin: .data(data),
                       timeout: deployTimeout, maxStdout: 256)
    }

    func ensureRunning() throws -> SSHRemoteStatus {
        do {
            return try status()
        } catch SSHRemoteError.ctl(code: 4, _) {
            _ = try stdout(of: SSHRemoteCommand.start, maxStdout: 256)
            return try waitForAgent()
        }
    }

    /// `status` until the agent answers (exit 4 means "not up yet").
    private func waitForAgent() throws -> SSHRemoteStatus {
        for attempt in 0..<startAttempts {
            do {
                return try status()
            } catch SSHRemoteError.ctl(code: 4, _) where attempt < startAttempts - 1 {
                if cancel?.isCancelled == true { throw SSHRemoteError.cancelled }
                Thread.sleep(forTimeInterval: startGap)
            }
        }
        throw SSHRemoteError.ctl(code: 4, detail: "")
    }
}
