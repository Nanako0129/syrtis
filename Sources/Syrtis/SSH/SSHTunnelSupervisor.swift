import Darwin
import Foundation

enum SSHTunnelStatus: Equatable, Sendable {
    case stopped
    case starting
    /// ssh is running and holds the local port (a bind on it fails).
    case up
    /// ssh exited; the next try is in `retryIn` seconds. `reason` is the first
    /// stderr line, masked, for the UI only.
    case backoff(attempt: Int, retryIn: TimeInterval, reason: String)
    /// Something else holds the machine's local port. The port is never
    /// changed silently: the UI asks the user to remove the machine and add it
    /// again.
    case portInUse(Int)
}

protocol SSHTunnelControlling: Sendable {
    func start(_ machine: SSHMachine)
    func stop(id: UUID)
    func status(id: UUID) -> SSHTunnelStatus
    /// Blocks until `up` or `portInUse`, or `timeout`; returns the status then.
    func waitUntilUp(id: UUID, timeout: TimeInterval) -> SSHTunnelStatus
}

/// uid and full argv of a live process.
struct SSHProcessInfo: Equatable, Sendable {
    let uid: uid_t
    let argv: [String]
}

enum SSHProcessInspector {
    static func info(pid: Int32) -> SSHProcessInfo? {
        guard pid > 1 else { return nil }
        var bsd = proc_bsdinfo()
        let size = Int32(MemoryLayout<proc_bsdinfo>.size)
        guard proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &bsd, size) == size else { return nil }
        guard let argv = argv(pid: pid) else { return nil }
        return SSHProcessInfo(uid: bsd.pbi_uid, argv: argv)
    }

    /// `KERN_PROCARGS2`: int32 argc, the exec path, NUL padding, then argc strings.
    static func argv(pid: Int32) -> [String]? {
        var mib: [Int32] = [CTL_KERN, KERN_PROCARGS2, pid]
        var length = 0
        guard sysctl(&mib, 3, nil, &length, nil, 0) == 0, length > 4 else { return nil }
        var buffer = [UInt8](repeating: 0, count: length)
        guard sysctl(&mib, 3, &buffer, &length, nil, 0) == 0, length > 4 else { return nil }
        let argc = buffer.withUnsafeBytes { Int($0.load(as: Int32.self)) }
        var index = 4
        while index < length, buffer[index] != 0 { index += 1 }
        while index < length, buffer[index] == 0 { index += 1 }
        var out: [String] = []
        while out.count < argc, index < length {
            var end = index
            while end < length, buffer[end] != 0 { end += 1 }
            out.append(String(decoding: buffer[index..<end], as: UTF8.self))
            index = end + 1
        }
        return out.count == argc ? out : nil
    }
}

/// One `ssh -N -L` per machine, restarted with backoff when it exits. Pids are
/// recorded under `stateDirectory`; at launch `reapStale()` ends only a process
/// of the same uid whose complete argv is what this supervisor would run.
final class SSHTunnelSupervisor: SSHTunnelControlling, @unchecked Sendable {
    struct Options: Sendable {
        var runner = SSHRunner()
        var stateDirectory: URL
        /// Delays between restarts; the last one repeats.
        var backoff: [TimeInterval] = [1, 2, 4, 8, 15, 30, 60]
        /// A tunnel that stayed up this long resets the backoff.
        var stableAfter: TimeInterval = 30
        var pollInterval: TimeInterval = 0.1
        var canBind: @Sendable (Int) -> Bool = SSHPorts.canBindLoopback
    }

    /// What the pid file holds: the child and the machine whose argv it runs.
    struct PidRecord: Codable {
        let pid: Int32
        let machine: SSHMachine
    }

    private final class Entry: @unchecked Sendable {
        let machine: SSHMachine
        let stopped = DispatchSemaphore(value: 0)
        let finished = DispatchSemaphore(value: 0)
        private let lock = NSLock()
        private var stopFlag = false
        private var current: SSHTunnelStatus = .starting

        init(machine: SSHMachine) { self.machine = machine }

        var isStopping: Bool {
            lock.lock()
            defer { lock.unlock() }
            return stopFlag
        }

        func requestStop() {
            lock.lock()
            stopFlag = true
            lock.unlock()
            stopped.signal()
        }

        var status: SSHTunnelStatus {
            get { lock.lock(); defer { lock.unlock() }; return current }
            set { lock.lock(); current = newValue; lock.unlock() }
        }
    }

    private let options: Options
    private let lock = NSLock()
    private var entries: [UUID: Entry] = [:]
    var onStatusChange: (@Sendable (UUID, SSHTunnelStatus) -> Void)?

    init(options: Options) { self.options = options }

    // MARK: Public

    func status(id: UUID) -> SSHTunnelStatus {
        lock.lock()
        defer { lock.unlock() }
        return entries[id]?.status ?? .stopped
    }

    /// Starts the machine's tunnel; a running one with the same spec is kept,
    /// one with another spec is replaced.
    func start(_ machine: SSHMachine) {
        lock.lock()
        let existing = entries[machine.id]
        if let existing, existing.machine == machine, !existing.isStopping {
            lock.unlock()
            return
        }
        let entry = Entry(machine: machine)
        entries[machine.id] = entry
        lock.unlock()
        if let existing { Self.end(existing) }
        let thread = Thread { [self] in
            run(entry)
            entry.finished.signal()
        }
        thread.name = "ssh-tunnel"
        thread.start()
    }

    func stop(id: UUID) {
        lock.lock()
        let entry = entries.removeValue(forKey: id)
        lock.unlock()
        if let entry { Self.end(entry) }
    }

    /// App termination: every tunnel and its ssh child.
    func stopAll() {
        lock.lock()
        let all = entries.values
        entries.removeAll()
        lock.unlock()
        for entry in all { entry.requestStop() }
        for entry in all { _ = entry.finished.wait(timeout: .now() + 5) }
    }

    /// Waits until the tunnel is `up`, or reports why it is not.
    func waitUntilUp(id: UUID, timeout: TimeInterval) -> SSHTunnelStatus {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let current = status(id: id)
            if case .up = current { return current }
            if case .portInUse = current { return current }
            Thread.sleep(forTimeInterval: options.pollInterval)
        }
        return status(id: id)
    }

    private static func end(_ entry: Entry) {
        entry.requestStop()
        _ = entry.finished.wait(timeout: .now() + 5)
    }

    // MARK: Stale processes

    /// Ends what an earlier run of the app left: for every pid file, a process
    /// of this uid whose full argv equals the argv recorded for it. Anything
    /// else is left alone and only the file is dropped. Returns the pids ended.
    @discardableResult
    func reapStale(uid: uid_t = getuid()) -> [Int32] {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: options.stateDirectory, includingPropertiesForKeys: nil)) ?? []
        var ended: [Int32] = []
        for file in files where file.pathExtension == "pid" {
            defer { try? FileManager.default.removeItem(at: file) }
            guard let data = try? Data(contentsOf: file),
                  let record = try? JSONDecoder().decode(PidRecord.self, from: data),
                  let machine = record.machine.validated(),
                  let args = try? options.runner.tunnelArguments(machine),
                  let info = SSHProcessInspector.info(pid: record.pid),
                  info.uid == uid, info.argv == [options.runner.sshPath] + args
            else { continue }
            if Self.terminate(pid: record.pid) { ended.append(record.pid) }
        }
        return ended
    }

    private static func terminate(pid: Int32) -> Bool {
        kill(pid, SIGTERM)
        for _ in 0..<40 {
            if kill(pid, 0) != 0 { return true }
            Thread.sleep(forTimeInterval: 0.05)
        }
        kill(pid, SIGKILL)
        return true
    }

    // MARK: Run loop

    private func pidFile(_ id: UUID) -> URL {
        options.stateDirectory.appendingPathComponent("\(id.uuidString).pid")
    }

    private func set(_ entry: Entry, _ status: SSHTunnelStatus) {
        guard entry.status != status else { return }
        entry.status = status
        onStatusChange?(entry.machine.id, status)
    }

    /// Sleeps up to `seconds`, or until stop is requested. True when stopped.
    private func nap(_ entry: Entry, _ seconds: TimeInterval) -> Bool {
        entry.stopped.wait(timeout: .now() + seconds) == .success || entry.isStopping
    }

    private func run(_ entry: Entry) {
        let machine = entry.machine
        let runner = options.runner
        try? FileManager.default.createDirectory(
            at: options.stateDirectory, withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700])
        guard let args = try? runner.tunnelArguments(machine) else {
            set(entry, .backoff(attempt: 1, retryIn: 0, reason: "invalid machine"))
            return
        }
        var attempt = 0
        while !entry.isStopping {
            if !options.canBind(machine.localPort) {
                set(entry, .portInUse(machine.localPort))
                if nap(entry, delay(max(attempt, 1))) { break }
                attempt += 1
                continue
            }
            set(entry, .starting)
            let process = Process()
            process.executableURL = URL(fileURLWithPath: runner.sshPath)
            process.arguments = args
            process.environment = runner.environment()
            process.standardInput = FileHandle.nullDevice
            process.standardOutput = FileHandle.nullDevice
            let errPipe = Pipe()
            process.standardError = errPipe
            do { try process.run() } catch {
                attempt += 1
                set(entry, .backoff(attempt: attempt, retryIn: delay(attempt), reason: "ssh did not start"))
                if nap(entry, delay(attempt)) { break }
                continue
            }
            try? errPipe.fileHandleForWriting.close()
            let tail = StderrTail()
            let reader = DispatchGroup()
            reader.enter()
            let errFd = errPipe.fileHandleForReading.fileDescriptor
            DispatchQueue.global(qos: .utility).async {
                var buffer = [UInt8](repeating: 0, count: 4096)
                while true {
                    let n = buffer.withUnsafeMutableBytes { read(errFd, $0.baseAddress, $0.count) }
                    if n < 0, errno == EINTR { continue }
                    if n <= 0 { break }
                    tail.append(buffer[0..<n])
                }
                reader.leave()
            }
            if let data = try? JSONEncoder().encode(PidRecord(pid: process.processIdentifier, machine: machine)) {
                try? data.write(to: pidFile(machine.id), options: .atomic)
            }
            var upSince: Date?
            while process.isRunning, !entry.isStopping {
                if upSince == nil, !options.canBind(machine.localPort) {
                    upSince = Date()
                    set(entry, .up)
                }
                Thread.sleep(forTimeInterval: options.pollInterval)
            }
            if process.isRunning {
                process.terminate()
                for _ in 0..<40 where process.isRunning { Thread.sleep(forTimeInterval: 0.05) }
                if process.isRunning { kill(process.processIdentifier, SIGKILL) }
                process.waitUntilExit()
            }
            _ = reader.wait(timeout: .now() + 2)
            try? FileManager.default.removeItem(at: pidFile(machine.id))
            if entry.isStopping { break }
            if let upSince, Date().timeIntervalSince(upSince) >= options.stableAfter { attempt = 0 }
            let line = SSHInput.firstLine(tail.data)
            if line.lowercased().contains("address already in use") {
                set(entry, .portInUse(machine.localPort))
            } else {
                attempt += 1
                set(entry, .backoff(attempt: attempt, retryIn: delay(attempt), reason: line))
            }
            sshLog.error("tunnel exited: \(line, privacy: .private)")
            if nap(entry, delay(max(attempt, 1))) { break }
        }
        set(entry, .stopped)
    }

    private func delay(_ attempt: Int) -> TimeInterval {
        let list = options.backoff
        guard !list.isEmpty else { return 1 }
        return list[min(max(attempt, 1) - 1, list.count - 1)]
    }

    private final class StderrTail: @unchecked Sendable {
        private let lock = NSLock()
        private var bytes = Data()

        func append(_ chunk: ArraySlice<UInt8>) {
            lock.lock()
            if bytes.count < 4096 { bytes.append(contentsOf: chunk.prefix(4096 - bytes.count)) }
            lock.unlock()
        }

        var data: Data {
            lock.lock()
            defer { lock.unlock() }
            return bytes
        }
    }
}
