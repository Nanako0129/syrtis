import Foundation
import TokenBarCore

/// When and where the embedded remote-usage agent starts. The agent itself
/// (pairing, grants, sharing) lives in Rust (`remote_agent.rs`); this only
/// decides the root and calls `tb_remote_start` / `tb_remote_stop`.
///
/// The app starts it once the launch-time scan registry is installed
/// (`ExtraScanRoots.apply`'s completion). Each owner-input setter that
/// succeeds calls `inputsChanged()` afterwards: `tb_remote_start` re-reads the
/// scan inputs and restarts the agent only when they changed, so a root the
/// owner removed is not shared until the next launch.
enum RemoteAgentLaunch {
    nonisolated static let isDebugBuild: Bool = {
        #if DEBUG
        true
        #else
        false
        #endif
    }()

    /// nil = the real agent root (`~/Library/Application Support/
    /// com.nyanako.syrtis.agent`). Only the shipping bundle (the official
    /// identifier and none of the test or demo flags, `BuildIdentity.shipping`)
    /// in a non-DEBUG build gets nil; everything else — a bare `swift run`,
    /// `make run`, the selftest, a bundle with a temporary identifier, a DEBUG
    /// build — gets `/tmp/syrtis-agent-dev-<uid>/<bundle id or bare>`, so a
    /// development build never writes the real root. Fail-closed: any input
    /// that is not positively the shipping build is a development build.
    /// `/tmp` keeps the control socket path under the 104-byte `sun_path`;
    /// it is emptied on reboot, so development pairings do not survive one.
    nonisolated static func homeOverride(
        shipping: BuildIdentity? = BuildIdentity.shipping(),
        debugBuild: Bool = isDebugBuild,
        bundleIdentifier: String? = Bundle.main.bundleIdentifier,
        uid: uid_t = getuid()
    ) -> String? {
        if shipping != nil, !debugBuild { return nil }
        return "/tmp/syrtis-agent-dev-\(uid)/\(directoryName(bundleIdentifier))"
    }

    /// One path component: characters outside `[A-Za-z0-9.-]` become `_`,
    /// and nothing that names the directory itself or its parent.
    nonisolated static func directoryName(_ bundleIdentifier: String?) -> String {
        let allowed = Set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-")
        let name = String((bundleIdentifier ?? "").map { allowed.contains($0) ? $0 : "_" })
        return name.isEmpty || name == "." || name == ".." ? "bare" : name
    }

    /// The one start path the app and the selftest share. Blocking.
    @discardableResult
    nonisolated static func start(home: String?) throws -> RemoteAgentStatus {
        try TBCore.remoteStart(homeOverride: home)
    }

    /// Blocking; the app calls it while terminating.
    nonisolated static func stop() {
        _ = try? TBCore.remoteStop()
    }

    @MainActor private static var launched = false

    /// The launch-time start, off the main thread. A failure (another process
    /// holds the agent root, an unsafe root) leaves the app as it was.
    @MainActor static func launch() {
        launched = true
        restartIfNeeded()
    }

    /// After an owner-input setter succeeded. Before `launch` this does
    /// nothing: the launch start reads the inputs itself.
    nonisolated static func inputsChanged() {
        Task { @MainActor in
            if launched { restartIfNeeded() }
        }
    }

    @MainActor private static func restartIfNeeded() {
        Task.detached(priority: .utility) {
            _ = try? start(home: homeOverride())
        }
    }
}
