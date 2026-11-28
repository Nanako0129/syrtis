import Darwin
import Foundation
import TokenBarCore

/// The saved machine list, in UserDefaults. No secret is stored; every record is
/// validated again on load, and one that fails is dropped rather than used.
final class SSHMachines: @unchecked Sendable {
    static let machinesKey = "ssh.machines.v1"

    private let defaults: UserDefaults
    private let lock = NSLock()

    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    // MARK: Machines

    func all() -> [SSHMachine] {
        lock.lock()
        defer { lock.unlock() }
        return (try? loadMachines()) ?? []
    }

    func machine(id: UUID) -> SSHMachine? { all().first { $0.id == id } }

    /// Fails when the same user, host and ssh port are already saved, or the
    /// local port is taken by another machine.
    func add(_ machine: SSHMachine) throws {
        lock.lock()
        defer { lock.unlock() }
        guard let clean = machine.validated() else { throw SSHRemoteError.invalidInput("machine") }
        var list = try loadMachines()
        guard !list.contains(where: {
            $0.id == clean.id
                || ($0.user == clean.user && $0.host == clean.host && $0.sshPort == clean.sshPort)
        }) else { throw SSHRemoteError.duplicateMachine }
        guard !list.contains(where: { $0.localPort == clean.localPort }) else {
            throw SSHRemoteError.portInUse(clean.localPort)
        }
        list.append(clean)
        saveMachines(list)
    }

    func update(_ machine: SSHMachine) throws {
        lock.lock()
        defer { lock.unlock() }
        guard let clean = machine.validated() else { throw SSHRemoteError.invalidInput("machine") }
        var list = try loadMachines()
        guard let index = list.firstIndex(where: { $0.id == clean.id }) else {
            throw SSHRemoteError.invalidInput("machine")
        }
        list[index] = clean
        saveMachines(list)
    }

    func remove(id: UUID) {
        lock.lock()
        defer { lock.unlock() }
        if let list = try? loadMachines() { saveMachines(list.filter { $0.id != id }) }
    }

    /// A record that does not decode is skipped. A store that is not a list at
    /// all throws, so a write never replaces it with an empty one.
    private func loadMachines() throws -> [SSHMachine] {
        guard let data = defaults.data(forKey: Self.machinesKey) else { return [] }
        guard let raw = try? JSONDecoder().decode([Lossy].self, from: data) else {
            throw SSHRemoteError.invalidInput("saved machines")
        }
        var seen: Set<Int> = []
        return raw.compactMap { $0.machine?.validated() }.filter { seen.insert($0.localPort).inserted }
    }

    private struct Lossy: Decodable {
        let machine: SSHMachine?
        init(from decoder: Decoder) { machine = try? SSHMachine(from: decoder) }
    }

    private func saveMachines(_ list: [SSHMachine]) {
        defaults.set(try? JSONEncoder().encode(list), forKey: Self.machinesKey)
    }

    // MARK: Ports

    /// A local port in 47100-47999 that binds right now, that no saved machine
    /// holds, and that is not the local agent's own listener. Persisted by the
    /// caller: `PeerRecord` pins host:port, so it must not change afterwards.
    func allocateLocalPort(
        avoiding extra: Set<Int> = [], canBind: (Int) -> Bool = SSHPorts.canBindLoopback
    ) throws -> Int {
        let taken = Set(all().map(\.localPort)).union(extra)
        for port in SSHInput.portRange.shuffled() where !taken.contains(port) && canBind(port) {
            return port
        }
        throw SSHRemoteError.noFreePort
    }
}

enum SSHPorts {
    /// Real `bind`s on 127.0.0.1:port and on 0.0.0.0:port, closed again, both
    /// with `SO_REUSEADDR` so a port left only in TIME_WAIT counts as free.
    /// False when either is held (by anything, including a running forward of
    /// ours): on XNU `SO_REUSEADDR` lets 127.0.0.1 bind beside a `*:port`
    /// listener, which the wildcard bind catches.
    static func canBindLoopback(_ port: Int) -> Bool {
        canBind(port, host: "127.0.0.1") && canBind(port, host: "0.0.0.0")
    }

    private static func canBind(_ port: Int, host: String) -> Bool {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, socklen_t(MemoryLayout<Int32>.size))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = in_port_t(UInt16(truncatingIfNeeded: port)).bigEndian
        address.sin_addr = in_addr(s_addr: inet_addr(host))
        let result = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        return result == 0
    }
}

extension SSHMachines {
    /// True when the machine's peer is no longer usable on this Mac: the
    /// producer revoked or expired the grant (a newer approval or `sharing off`
    /// there ends it, which the receiver sees as `revoked`). The UI shows
    /// "needs re-approval" and calls `SSHMachineCoordinator.reapprove`.
    static func needsReapproval(_ machine: SSHMachine, peers: [RemotePeer]) -> Bool {
        guard let key = machine.producerStatic else { return false }
        guard let peer = peers.first(where: { $0.producerStatic == key }) else { return true }
        return peer.state != "active"
    }
}

/// How SSH machines' peers are shown. Their peer address is the local end of
/// a tunnel (`127.0.0.1:port`), so the user sees the machine's label instead
/// and manages them only in the SSH machines section.
enum SSHPeerNames {
    /// True when the peer is one an SSH machine owns (its key, or its
    /// fingerprint, matches a saved machine).
    static func isSSH(_ peer: RemotePeer, machines: [SSHMachine]) -> Bool {
        machines.contains { $0.producerStatic == peer.producerStatic || $0.fingerprint == peer.fingerprint }
    }

    /// The peers that belong in "linked devices": everything but the SSH machines'.
    static func linked(_ peers: [RemotePeer], machines: [SSHMachine]) -> [RemotePeer] {
        peers.filter { !isSSH($0, machines: machines) }
    }

    /// Fingerprint -> machine label for every saved machine, for the name
    /// lookups that are keyed by fingerprint. Peers matched by `producerStatic`
    /// add their own fingerprint.
    static func names(machines: [SSHMachine], peers: [RemotePeer]) -> [String: String] {
        var result: [String: String] = [:]
        for machine in machines {
            if let fingerprint = machine.fingerprint { result[fingerprint] = machine.label }
            if let key = machine.producerStatic,
               let peer = peers.first(where: { $0.producerStatic == key }) {
                result[peer.fingerprint] = machine.label
            }
        }
        return result
    }

    /// `learned` with the SSH machines' labels laid over it.
    static func overlaid(
        _ learned: [String: String], machines: [SSHMachine], peers: [RemotePeer]
    ) -> [String: String] {
        learned.merging(names(machines: machines, peers: peers)) { _, label in label }
    }
}
