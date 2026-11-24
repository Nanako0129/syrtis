import Foundation
import TokenBarCore

/// The app's one SSH machine runtime: the saved list, the tunnel supervisor and
/// the coordinator the Devices page drives. `agentStarted()` runs after the
/// embedded agent is up (`RemoteAgentLaunch`), `stop()` while terminating.
final class SSHRemoteRuntime: @unchecked Sendable {
    static let shared = SSHRemoteRuntime()

    let store: SSHMachines
    let supervisor: SSHTunnelSupervisor
    let coordinator: SSHMachineCoordinator

    private let lock = NSLock()
    private var launched = false

    init(
        store: SSHMachines = SSHMachines(), resources: URL? = Bundle.main.resourceURL,
        stateDirectory: URL = SSHRemoteRuntime.defaultStateDirectory()
    ) {
        self.store = store
        supervisor = SSHTunnelSupervisor(options: .init(stateDirectory: stateDirectory))
        let tunnels = supervisor
        coordinator = SSHMachineCoordinator(.init(
            store: store, tunnels: tunnels, local: LiveSSHLocalPairing(),
            makeRemote: { target, cancel in SSHCtl(target: target, cancel: cancel) },
            binaryFor: { SSHAgentPin.bundledBinary(triple: $0, resources: resources) },
            pin: SSHAgentPin.pin(resources: resources)))
    }

    /// Pid files of the tunnels, kept apart for a development build exactly as
    /// the agent's root is: `reapStale` ends processes by these files, and a
    /// development run must not end the installed app's tunnels.
    static func defaultStateDirectory() -> URL {
        if let home = RemoteAgentLaunch.homeOverride() {
            return URL(fileURLWithPath: home + "-ssh-tunnels", isDirectory: true)
        }
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return support.appendingPathComponent("com.nyanako.syrtis.ssh-tunnels", isDirectory: true)
    }

    /// After the embedded agent started (also after each restart over changed
    /// inputs). The first call ends what an earlier run left; every call makes
    /// sure each enabled machine has its forward (a running one is kept).
    func agentStarted() {
        lock.lock()
        let first = !launched
        launched = true
        lock.unlock()
        if first {
            supervisor.reapStale()
        }
        for machine in store.all() where machine.enabled { supervisor.start(machine) }
    }

    /// Blocking, bounded: the tunnels and their ssh children end.
    func stop() {
        lock.lock()
        launched = false
        lock.unlock()
        supervisor.stopAll()
    }
}
