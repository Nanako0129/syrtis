import CryptoKit
import Darwin
import Foundation

/// Copies the Mac's price table to each SSH machine's agent, which never fetches one itself
/// (docs/knowledge/remote-agent.md, "Price table on SSH machines"). Triggers: an add or
/// re-approval that completed (`force`), each time a machine's tunnel comes up, and an hourly
/// timer for machines whose tunnel is up. A failure is logged privately and changes nothing: the
/// machine's state is the tunnel's and the coordinator's alone, and the next trigger retries.
final class SSHPricingPush: @unchecked Sendable {
    /// The hourly trigger, and the window in which the same bytes are not sent to the same
    /// machine twice: a failing upload is not repeated at the tunnel's reconnect rate.
    static let period: TimeInterval = 3600
    /// A larger local file is not sent (the litellm table is about 5 MB).
    static let maxBytes = 32 << 20

    private let runner: SSHRunner
    private let store: SSHMachines
    private let source: URL
    private let lock = NSLock()
    private var inFlight: [UUID: SSHCancellation] = [:]
    /// Removed machines: nothing starts for them again (an add always makes a new id).
    private var gone: Set<UUID> = []
    /// Per machine and file: the local sha256 last sent (whatever the outcome) and when.
    private var attempts: [UUID: [SSHPricingFile: (digest: String, at: Date)]] = [:]
    private var timer: DispatchSourceTimer?
    /// The upload's timeout (deploy-length, not the 30 s of a `ctl` call).
    var uploadTimeout: TimeInterval = 300

    init(runner: SSHRunner = SSHRunner(), store: SSHMachines, source: URL = SSHPricingPush.localDirectory()) {
        self.runner = runner
        self.store = store
        self.source = source
    }

    // MARK: Local source

    /// The local agent's pricing directory, which `sync_pricing` (`remote_agent.rs`) keeps
    /// filled: `<home>/<state suffix>/pricing`, with the same root `RemoteAgentLaunch` passes
    /// to `tb_remote_start`. An override home has the suffix `a` (`OVERRIDE_SUFFIX`); the
    /// shipping root is below the directory-service home (`trusted_home`, getpwuid).
    static func localDirectory(home: String? = RemoteAgentLaunch.homeOverride()) -> URL {
        if let home { return URL(fileURLWithPath: home).appendingPathComponent("a/pricing", isDirectory: true) }
        let passwdHome = getpwuid(geteuid()).map { String(cString: $0.pointee.pw_dir) }
            ?? FileManager.default.homeDirectoryForCurrentUser.path
        return URL(fileURLWithPath: passwdHome)
            .appendingPathComponent("Library/Application Support/com.nyanako.syrtis.agent/pricing", isDirectory: true)
    }

    /// The file's bytes when it is a regular file (a link is not followed), at most `maxBytes`,
    /// and the envelope the engine requires; else nil. One descriptor: what is checked is what is
    /// read, hashed and sent, though `sync_pricing` replaces the file every few minutes.
    static func readSource(_ url: URL) -> Data? {
        // O_NONBLOCK: opening a FIFO planted there does not wait for a writer.
        let fd = open(url.path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard fd >= 0 else { return nil }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        var info = stat()
        guard fstat(fd, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG, info.st_size <= maxBytes,
              let data = try? handle.read(upToCount: maxBytes + 1), data.count <= maxBytes,
              isEnvelope(data)
        else { return nil }
        return data
    }

    /// A JSON object with an unsigned integer `timestamp` and an object `data`
    /// (`CachedData` in tokscale-core's pricing cache).
    static func isEnvelope(_ data: Data) -> Bool {
        guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              object["data"] is [String: Any],
              let stamp = object["timestamp"] as? NSNumber,
              CFGetTypeID(stamp) != CFBooleanGetTypeID()
        else { return false }
        let type = String(cString: stamp.objCType)
        return type != "d" && type != "f" && stamp.int64Value >= 0
    }

    static func sha256(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    // MARK: Triggers

    /// A tunnel that comes up triggers a push; so does an hourly timer for every saved machine
    /// whose tunnel is up. The supervisor has one status observer and this is it.
    func observe(_ supervisor: SSHTunnelSupervisor) {
        supervisor.onStatusChange = { [weak self] id, status in
            if status == .up { self?.trigger(id) }
        }
        let timer = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
        timer.schedule(deadline: .now() + Self.period, repeating: Self.period)
        timer.setEventHandler { [weak self, weak supervisor] in
            guard let self, let supervisor else { return }
            for machine in self.store.all() where supervisor.status(id: machine.id) == .up { self.trigger(machine.id) }
        }
        timer.resume()
        lock.withLock { self.timer = timer }
    }

    /// One push off the caller's thread. `force` (an add or re-approval) ignores the hour window once.
    func trigger(_ id: UUID, force: Bool = false) {
        DispatchQueue.global(qos: .utility).async { [self] in push(id, force: force) }
    }

    /// The machine was removed: its push in flight is cancelled and none starts again.
    func forget(_ id: UUID) {
        let cancel = lock.withLock { () -> SSHCancellation? in
            gone.insert(id)
            attempts[id] = nil
            return inFlight[id]
        }
        cancel?.cancel()
    }

    // MARK: Push

    /// Blocking. Nothing when the machine is not saved, removed, or already pushing. Reads both
    /// local files, drops those sent with the same bytes within the hour (unless `force`), asks the
    /// remote for its hashes (74 or 75 there: skip all), and uploads each file whose remote hash
    /// differs or is unknown. Returns how many uploads were started (selftest).
    @discardableResult
    func push(_ id: UUID, force: Bool = false, now: Date = Date()) -> Int {
        let cancel = SSHCancellation()
        let started = lock.withLock { () -> Bool in
            guard !gone.contains(id), inFlight[id] == nil else { return false }
            inFlight[id] = cancel
            return true
        }
        guard started else { return 0 }
        defer { lock.withLock { inFlight[id] = nil } }
        guard let target = try? store.machine(id: id)?.target() else { return 0 }

        let recent = lock.withLock { attempts[id] ?? [:] }
        var pending: [(file: SSHPricingFile, data: Data, digest: String)] = []
        for file in SSHPricingFile.allCases {
            guard let data = Self.readSource(source.appendingPathComponent(file.rawValue)) else { continue }
            let digest = Self.sha256(data)
            if !force, let last = recent[file], last.digest == digest, now.timeIntervalSince(last.at) < Self.period {
                continue
            }
            pending.append((file, data, digest))
        }
        guard !pending.isEmpty else { return 0 }

        var ctl = SSHCtl(runner: runner, target: target, cancel: cancel)
        ctl.deployTimeout = uploadTimeout
        let remote: [SSHPricingFile: String]
        do {
            remote = try ctl.pricingHashes() ?? [:]
        } catch {
            sshLog.error("price table: hash check failed: \(String(describing: error), privacy: .private)")
            return 0
        }
        var uploads = 0
        for item in pending where remote[item.file] != item.digest {
            guard !cancel.isCancelled, store.machine(id: id) != nil else { break }
            lock.withLock { attempts[id, default: [:]][item.file] = (item.digest, now) }
            uploads += 1
            do {
                try ctl.uploadPricing(item.file, data: item.data, digest: item.digest)
            } catch {
                sshLog.error("price table: upload failed: \(String(describing: error), privacy: .private)")
            }
        }
        return uploads
    }
}
