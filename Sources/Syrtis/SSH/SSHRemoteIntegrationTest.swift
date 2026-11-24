import Darwin
import Foundation
import TokenBarCore

/// A loopback TCP forwarder standing in for `ssh -L 127.0.0.1:lport:127.0.0.1:rport`.
final class SSHLoopbackForwarder: @unchecked Sendable {
    let localPort: Int
    let remotePort: Int
    private let listener: Int32
    private let lock = NSLock()
    private var open: [Int32] = []
    private var cutFlag = false

    init?(localPort: Int, remotePort: Int) {
        self.localPort = localPort
        self.remotePort = remotePort
        listener = socket(AF_INET, SOCK_STREAM, 0)
        var reuse: Int32 = 1
        setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &reuse, socklen_t(MemoryLayout<Int32>.size))
        var address = Self.address(localPort)
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(listener, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
        }
        guard bound == 0, listen(listener, 16) == 0 else {
            close(listener)
            return nil
        }
        Thread.detachNewThread { [self] in accept() }
    }

    fileprivate static func address(_ port: Int) -> sockaddr_in {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = in_port_t(UInt16(port)).bigEndian
        address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
        return address
    }

    private func track(_ fd: Int32) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if cutFlag { return false }
        open.append(fd)
        return true
    }

    /// A closed descriptor number may be reused: `cut` must not shut it down.
    private func untrack(_ fd: Int32) {
        lock.lock()
        open.removeAll { $0 == fd }
        lock.unlock()
    }

    private let closed = DispatchSemaphore(value: 0)

    /// Polls so `cut` can close the listener from this thread: closing a socket
    /// another thread is blocked in `accept` on would keep the port held.
    private func accept() {
        while true {
            lock.lock()
            let stop = cutFlag
            lock.unlock()
            if stop {
                close(listener)
                closed.signal()
                return
            }
            var pending = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
            if poll(&pending, 1, 50) <= 0 { continue }
            let client = Darwin.accept(listener, nil, nil)
            if client < 0 { continue }
            let upstream = socket(AF_INET, SOCK_STREAM, 0)
            var target = Self.address(remotePort)
            let connected = withUnsafePointer(to: &target) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(upstream, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
            }
            guard connected == 0, track(client), track(upstream) else {
                close(client)
                close(upstream)
                continue
            }
            // A half-close (the client sends its request and shuts its write side) is
            // passed on; the sockets close when both directions are done.
            let remaining = Remaining()
            for (from, to) in [(client, upstream), (upstream, client)] {
                Thread.detachNewThread {
                    var buffer = [UInt8](repeating: 0, count: 16 * 1024)
                    while true {
                        let n = read(from, &buffer, buffer.count)
                        if n <= 0 { break }
                        var sent = 0
                        while sent < n {
                            // `&buffer[sent]` would pass a one-element temporary in a debug build.
                            let w = buffer.withUnsafeBytes { write(to, $0.baseAddress! + sent, n - sent) }
                            if w <= 0 { break }
                            sent += w
                        }
                    }
                    shutdown(to, SHUT_WR)
                    if remaining.finishOne() {
                        self.untrack(client)
                        self.untrack(upstream)
                        close(client)
                        close(upstream)
                    }
                }
            }
        }
    }

    private final class Remaining: @unchecked Sendable {
        private let lock = NSLock()
        private var left = 2

        func finishOne() -> Bool {
            lock.lock()
            defer { lock.unlock() }
            left -= 1
            return left == 0
        }
    }

    /// Stops listening and drops every live connection: what a dead ssh does.
    func cut() {
        lock.lock()
        cutFlag = true
        let all = open
        open.removeAll()
        lock.unlock()
        _ = closed.wait(timeout: .now() + 2)
        for fd in all { shutdown(fd, SHUT_RDWR) }
    }
}

final class SSHLoopbackTunnels: SSHTunnelControlling, @unchecked Sendable {
    private let lock = NSLock()
    private var forwarders: [UUID: SSHLoopbackForwarder] = [:]

    func start(_ machine: SSHMachine) {
        lock.lock()
        defer { lock.unlock() }
        if let old = forwarders[machine.id], old.localPort == machine.localPort, old.remotePort == machine.remotePort { return }
        forwarders[machine.id]?.cut()
        forwarders[machine.id] = SSHLoopbackForwarder(localPort: machine.localPort, remotePort: machine.remotePort)
    }

    func stop(id: UUID) {
        lock.lock()
        defer { lock.unlock() }
        forwarders.removeValue(forKey: id)?.cut()
    }

    func status(id: UUID) -> SSHTunnelStatus {
        lock.lock()
        defer { lock.unlock() }
        return forwarders[id] == nil ? .portInUse(0) : .up
    }

    func waitUntilUp(id: UUID, timeout: TimeInterval) -> SSHTunnelStatus { status(id: id) }

    func cutAll() {
        lock.lock()
        defer { lock.unlock() }
        for forwarder in forwarders.values { forwarder.cut() }
    }
}

/// Manual lane (`--selftest-ssh` with `SYRTIS_SSH_IT_AGENT` set): a real
/// loopback `syrtis-agent` (a debug build of the agent with `ctl`) is the "remote",
/// the fake ssh runs the real command strings against it, a TCP forwarder
/// replaces `ssh -L`, and the app's embedded agent is the receiver. Skipped
/// when the variable is not set.
enum SSHRemoteIntegrationTest {
    static func run(expect: (Bool, String) -> Void) {
        guard let path = ProcessInfo.processInfo.environment["SYRTIS_SSH_IT_AGENT"],
              FileManager.default.isExecutableFile(atPath: path)
        else {
            print("skip SSH integration: set SYRTIS_SSH_IT_AGENT to a debug build of syrtis-agent that has the `ctl` subcommands")
            return
        }
        guard let fake = try? SSHFake(agentBinary: URL(fileURLWithPath: path), short: true, realAgentHome: true) else {
            return expect(false, "SSH integration: setup")
        }
        let receiverHome = fake.dir.appendingPathComponent("r").path
        defer {
            RemoteAgentLaunch.stop()
            fake.cleanup()
        }
        guard (try? TBCore.remoteStart(homeOverride: receiverHome))?.running == true else {
            return expect(false, "SSH integration: the receiver agent did not start")
        }
        let suite = "com.tokenbar.selftest.ssh-it"
        guard let defaults = UserDefaults(suiteName: suite) else { return }
        defaults.removePersistentDomain(forName: suite)
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = SSHMachines(defaults: defaults)
        let tunnels = SSHLoopbackTunnels()
        let runner = fake.runner
        let binary = fake.uploaded
        var deps = SSHMachineCoordinator.Dependencies(
            store: store, tunnels: tunnels, local: LiveSSHLocalPairing(),
            makeRemote: { target, cancel in
                var ctl = SSHCtl(runner: runner, target: target, cancel: cancel)
                ctl.startGap = 0.2
                return ctl
            },
            binaryFor: { _ in binary })
        deps.pin = fake.agentSHA256
        let coordinator = SSHMachineCoordinator(deps)
        guard let target = try? SSHTarget(user: "me", host: "box.example") else { return }
        let ctl = SSHCtl(runner: runner, target: target)
        // Whatever happens below, stop the agent the deploy script started.
        defer { _ = try? ctl.shutdown() }

        // Add through deploy, ctl, claim and approve.
        let added = SSHRemoteSelfTest.blocking {
            try await coordinator.add(user: "me", host: "box.example", label: "it", confirm: { _ in true })
        }
        guard let machine = SSHRemoteSelfTest.value(added), let key = machine.producerStatic, let grantId = machine.grantId else {
            return expect(false, "SSH integration: add claimed, approved over ctl and paired (\(String(describing: SSHRemoteSelfTest.errorOf(added))))")
        }
        let status = try? ctl.status()
        let peers = (try? TBCore.remotePeers()) ?? []
        expect(status?.activeGrantCount == 1 && status?.sharingEnabled == true && status?.bindHost == "127.0.0.1"
            && peers.contains { $0.producerStatic == key && $0.state == "active" && $0.port == machine.localPort && $0.host == "127.0.0.1" },
            "SSH integration: the real agent has one grant and sharing on; the peer is pinned to 127.0.0.1:lport")
        expect(!fake.invocations.flatMap { $0 }.contains { $0.contains("syrtis://") },
               "SSH integration: no ssh argv carried a Pair URL")

        // Pull through the forwarder.
        let selection = DeviceSelection(local: false, peers: [key])
        let first = try? TBCore.graphDevices(selection: selection, mode: .refresh)
        let row = first?.devices.first { $0.state != "local" }
        expect(row?.state == "fresh", "SSH integration: a refresh through the forwarder is fresh (state \(row?.state ?? "none"), reason \(row?.reason ?? "-"))")

        // Cut the forwarder: refresh answers last good or stale within the existing 135 s fetch deadline.
        tunnels.cutAll()
        let started = Date()
        let second = try? TBCore.graphDevices(selection: selection, mode: .refresh)
        let elapsed = Date().timeIntervalSince(started)
        let cutRow = second?.devices.first { $0.state != "local" }
        expect(["lastGood", "stale"].contains(cutRow?.state ?? "") && elapsed < 135,
               "SSH integration: after the forwarder is cut a refresh is lastGood or stale in \(Int(elapsed)) s (state \(cutRow?.state ?? "none"), reason \(cutRow?.reason ?? "-"))")

        // The derived grant id is the real one: revoking it drops the count and leaves sharing on.
        try? ctl.revokeGrant(id: grantId)
        let after = try? ctl.status()
        expect(after?.activeGrantCount == 0 && after?.sharingEnabled == true,
               "SSH integration: ctl grant revoke with the derived grant id revokes exactly that grant")

        // Remove: sharing goes off, the peer is forgotten.
        let removed = SSHRemoteSelfTest.value(SSHRemoteSelfTest.blocking { try await coordinator.remove(id: machine.id) })
        let final = try? ctl.status()
        expect(removed?.manualCommands.isEmpty == true && final?.sharingEnabled == false && final?.activeGrantCount == 0
            && store.all().isEmpty && !((try? TBCore.remotePeers()) ?? []).contains { $0.producerStatic == key },
            "SSH integration: remove leaves sharing off, no grant, no peer and no saved machine")
    }
}
