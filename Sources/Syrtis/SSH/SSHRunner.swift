import Darwin
import Foundation

/// Cooperative cancel flag shared between a flow and the runner it drives.
final class SSHCancellation: @unchecked Sendable {
    private let lock = NSLock()
    private var flag = false

    func cancel() {
        lock.lock()
        flag = true
        lock.unlock()
    }

    var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return flag
    }
}

enum SSHStdin: Sendable {
    case none
    case data(Data)
    /// Streamed in chunks (the agent binary).
    case file(URL)
}

struct SSHRunResult: Sendable {
    enum End: Equatable, Sendable {
        case exited(Int32)
        case timedOut
        case outputLimit
        case cancelled
    }

    let end: End
    let stdout: Data
    /// Capped at `SSHRunner.stderrLimit`; only its first line is ever shown.
    let stderr: Data
}

/// Runs `/usr/bin/ssh` with the plan's fixed options and a program-owned remote
/// command. Blocking: call off the main thread.
struct SSHRunner: Sendable {
    /// Every command (plan, "SSH 選項"). The user's `~/.ssh/config` and
    /// ssh-agent still apply; these options override what it could weaken.
    static let fixedOptions: [String] = [
        "-T",
        "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=yes",
        "-o", "ForwardAgent=no",
        "-o", "ForwardX11=no",
        "-o", "PermitLocalCommand=no",
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "ConnectTimeout=10",
        "-o", "NoHostAuthenticationForLocalhost=no",
        "-o", "ForkAfterAuthentication=no",
        "-o", "RemoteCommand=none",
    ]
    /// One-shot commands only: a `-L` on the tunnel would be cleared by this.
    /// `SessionType=default` runs our command even if the config asks for none.
    static let oneShotOptions: [String] = ["-o", "ClearAllForwardings=yes", "-o", "SessionType=default"]
    static let tunnelOptions: [String] = [
        "-N",
        "-o", "SessionType=none",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ServerAliveInterval=30",
    ]
    static let stderrLimit = 8 * 1024

    var sshPath = "/usr/bin/ssh"
    /// Added to the child environment (tests); the rest is an allowlist.
    var extraEnvironment: [String: String] = [:]

    static let inheritedEnvironment = [
        "HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LC_ALL", "SSH_AUTH_SOCK",
    ]

    func environment() -> [String: String] {
        let source = ProcessInfo.processInfo.environment
        var env: [String: String] = [:]
        for key in Self.inheritedEnvironment { env[key] = source[key] }
        return env.merging(extraEnvironment) { _, new in new }
    }

    func oneShotArguments(_ target: SSHTarget, _ command: SSHRemoteCommand) -> [String] {
        Self.fixedOptions + Self.oneShotOptions
            + target.portArguments + ["--", target.destination, command.shell]
    }

    /// `ssh -N -L 127.0.0.1:lport:127.0.0.1:rport` for one machine.
    func tunnelArguments(_ machine: SSHMachine) throws -> [String] {
        let target = try machine.target()
        return Self.fixedOptions + Self.tunnelOptions
            + target.portArguments
            + ["-L", "127.0.0.1:\(machine.localPort):127.0.0.1:\(machine.remotePort)",
               "--", target.destination]
    }

    /// Output over `maxStdout` bytes, or running past `timeout`, ends the child
    /// and is reported as such. stderr is drained and capped.
    func run(
        _ target: SSHTarget, _ command: SSHRemoteCommand, stdin: SSHStdin = .none,
        timeout: TimeInterval = 30, maxStdout: Int = 256 * 1024, cancel: SSHCancellation? = nil
    ) throws -> SSHRunResult {
        try Self.execute(
            path: sshPath, arguments: oneShotArguments(target, command), environment: environment(),
            stdin: stdin, timeout: timeout, maxStdout: maxStdout, cancel: cancel)
    }

    // MARK: Process plumbing

    private final class Capture: @unchecked Sendable {
        private let lock = NSLock()
        private(set) var data = Data()
        private(set) var overflowed = false
        let limit: Int
        let keepDraining: Bool

        init(limit: Int, keepDraining: Bool) {
            self.limit = limit
            self.keepDraining = keepDraining
        }

        /// False once the limit is passed and the reader should stop.
        func append(_ chunk: UnsafeBufferPointer<UInt8>) -> Bool {
            lock.lock()
            defer { lock.unlock() }
            let room = limit - data.count
            if chunk.count > room {
                data.append(contentsOf: chunk.prefix(max(room, 0)))
                overflowed = true
                return keepDraining
            }
            data.append(contentsOf: chunk)
            return true
        }

        var snapshot: (Data, Bool) {
            lock.lock()
            defer { lock.unlock() }
            return (data, overflowed)
        }
    }

    static func execute(
        path: String, arguments: [String], environment: [String: String], stdin: SSHStdin,
        timeout: TimeInterval, maxStdout: Int, cancel: SSHCancellation?
    ) throws -> SSHRunResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = arguments
        process.environment = environment
        let outPipe = Pipe()
        let errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = errPipe
        var inPipe: Pipe?
        switch stdin {
        case .none: process.standardInput = FileHandle.nullDevice
        case .data, .file:
            let pipe = Pipe()
            // EPIPE as an error, never as a signal that ends the app.
            _ = fcntl(pipe.fileHandleForWriting.fileDescriptor, F_SETNOSIGPIPE, 1)
            process.standardInput = pipe
            inPipe = pipe
        }

        let exited = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in exited.signal() }
        do { try process.run() } catch { throw SSHRemoteError.launchFailed }
        try? outPipe.fileHandleForWriting.close()
        try? errPipe.fileHandleForWriting.close()
        try? inPipe?.fileHandleForReading.close()

        let out = Capture(limit: maxStdout, keepDraining: false)
        let err = Capture(limit: stderrLimit, keepDraining: true)
        let readers = DispatchGroup()
        func drain(_ handle: FileHandle, into capture: Capture) {
            readers.enter()
            let fd = handle.fileDescriptor
            DispatchQueue.global(qos: .userInitiated).async {
                var buffer = [UInt8](repeating: 0, count: 16 * 1024)
                while true {
                    let n = buffer.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
                    if n < 0, errno == EINTR { continue }
                    if n <= 0 { break }
                    let keep = buffer.withUnsafeBufferPointer {
                        capture.append(UnsafeBufferPointer(rebasing: $0[0..<n]))
                    }
                    if !keep { break }
                }
                readers.leave()
            }
        }
        drain(outPipe.fileHandleForReading, into: out)
        drain(errPipe.fileHandleForReading, into: err)

        if let inPipe {
            let fd = inPipe.fileHandleForWriting.fileDescriptor
            let writer = Thread {
                upload(stdin, to: fd)
                try? inPipe.fileHandleForWriting.close()
            }
            writer.start()
        }

        var end: SSHRunResult.End?
        let deadline = Date().addingTimeInterval(timeout)
        while exited.wait(timeout: .now() + .milliseconds(20)) == .timedOut {
            if cancel?.isCancelled == true { end = .cancelled }
            else if Date() >= deadline { end = .timedOut }
            else if out.snapshot.1 { end = .outputLimit }
            if end != nil {
                process.terminate()
                if exited.wait(timeout: .now() + 2) == .timedOut {
                    kill(process.processIdentifier, SIGKILL)
                    _ = exited.wait(timeout: .now() + 2)
                }
                break
            }
        }
        _ = readers.wait(timeout: .now() + 5)
        if end == nil {
            end = out.snapshot.1 ? .outputLimit
                : .exited(process.terminationReason == .exit ? process.terminationStatus : 255)
        }
        return SSHRunResult(end: end ?? .timedOut, stdout: out.snapshot.0, stderr: err.snapshot.0)
    }

    /// Writes `source` to `fd`; stops quietly on EPIPE (ssh died first).
    private static func upload(_ source: SSHStdin, to fd: Int32) {
        func writeAll(_ bytes: UnsafeRawBufferPointer) -> Bool {
            var offset = 0
            while offset < bytes.count {
                let n = write(fd, bytes.baseAddress! + offset, bytes.count - offset)
                if n < 0, errno == EINTR { continue }
                if n <= 0 { return false }
                offset += n
            }
            return true
        }
        switch source {
        case .none: return
        case .data(let data): _ = data.withUnsafeBytes { writeAll($0) }
        case .file(let url):
            guard let handle = try? FileHandle(forReadingFrom: url) else { return }
            defer { try? handle.close() }
            while let chunk = try? handle.read(upToCount: 64 * 1024), !chunk.isEmpty {
                if !chunk.withUnsafeBytes({ writeAll($0) }) { return }
            }
        }
    }
}
