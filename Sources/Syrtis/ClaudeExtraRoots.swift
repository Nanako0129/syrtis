import Foundation
import TokenBarCore

/// Persisted list of extra Claude config dirs the user configured for a
/// second (or further) `CLAUDE_CONFIG_DIR`-isolated account. Each entry
/// expands to the two sub-roots the engine actually scans for the primary
/// account — `<dir>/projects` and `<dir>/transcripts` (D2 in the extra-root
/// plan) — and is pushed to the Rust registry via `TBCore.setExtraScanPaths`.
///
/// Usage from these roots merges into the single reported total; there is no
/// per-account breakdown. Settings must say so explicitly (see
/// `SettingsPanel`'s hint copy) so a user adding a second account does not
/// expect a split view that does not exist.
enum ClaudeExtraRoots {
    static let storageKey = "tokenbar.claude.extraConfigDirs"
    /// Incremented once the core has accepted a new root set. Views observe
    /// THIS rather than `storageKey` when they need to reload against the new
    /// roots — see `apply`.
    static let generationKey = "tokenbar.claude.extraRootsGeneration"

    static func load() -> [String] {
        guard let raw = UserDefaults.standard.string(forKey: storageKey),
            let data = raw.data(using: .utf8),
            let paths = try? JSONDecoder().decode([String].self, from: data)
        else { return [] }
        return paths
    }

    static func save(_ paths: [String]) {
        let data = (try? JSONEncoder().encode(paths)) ?? Data("[]".utf8)
        UserDefaults.standard.set(String(data: data, encoding: .utf8) ?? "[]", forKey: storageKey)
    }

    /// A config dir is missing right now (external drive unmounted, typo,
    /// etc). Kept in the list either way — an unmounted volume must not
    /// silently drop the user's setting the way vendor's own scan does for
    /// paths that vanish mid-session; the difference is Settings can *show*
    /// the warning, where a background scan has nowhere to show it.
    ///
    /// Blocking: `fileExists` stats the path, which on a stalled network or
    /// external mount takes as long as the mount takes to time out. Call it
    /// off the main actor — `missingRoots(in:then:)` is the wrapper Settings
    /// uses.
    static func isMissing(_ path: String) -> Bool {
        var isDir: ObjCBool = false
        let exists = FileManager.default.fileExists(atPath: path, isDirectory: &isDir)
        return !exists || !isDir.boolValue
    }

    /// Reject the home directory itself and the filesystem root outright —
    /// anything else is the user's call (mirrors the plan's risk table: the
    /// core only warns on an out-of-home path, it does not block).
    static func isRejectedRoot(_ path: String) -> Bool {
        let standardized = (path as NSString).standardizingPath
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return standardized.isEmpty || standardized == "/" || standardized == home
    }

    /// Expand a user-entered `CLAUDE_CONFIG_DIR` into the two sub-roots the
    /// engine scans for the primary `$HOME/.claude` root (`clients.rs:187`,
    /// `scanner.rs:443`) — requiring only one sub-path here would leave the
    /// other silently missing whenever a real isolated dir produces it.
    static func expand(_ configDir: String) -> [String] {
        let base = (configDir as NSString).standardizingPath
        return ["\(base)/projects", "\(base)/transcripts"]
    }

    /// Expand and deduplicate the configured dirs into the exact list of scan
    /// roots the engine walks. Split out so `ExtraScanRoots` can merge this
    /// list with another client's roots without decoding the encoded payload.
    static func expandedPaths(_ configDirs: [String]) -> [String] {
        var seen = Set<String>()
        return configDirs.flatMap(expand).filter { seen.insert($0).inserted }
    }

    /// Build the `tb_set_extra_scan_paths` payload from the persisted list:
    /// `{"claude": [path, path, ...]}`, expanded and deduplicated. One call
    /// covers every configured dir — full-replace semantics, so an empty
    /// list clears the registry (the Settings rollback path).
    ///
    /// Claude-only, and kept byte-compatible: `ExtraScanRoots.payloadJSON`
    /// returns this exact string when no Pi or DSH roots are configured, so an
    /// upgrade does not look like a scan-root change.
    static func payloadJSON(_ configDirs: [String]) -> String {
        let object = ["claude": expandedPaths(configDirs)]
        let data = (try? JSONEncoder().encode(object)) ?? Data("{}".utf8)
        return String(data: data, encoding: .utf8) ?? "{}"
    }

    /// The `tb_set_claude_config_dirs` payload: the configured directories
    /// themselves, standardized, as a JSON array.
    ///
    /// Deliberately built from `load()` — the list the user configured — and
    /// not from `appliedPayloadJSON`, the scan subset the core accepted. They
    /// answer different questions: a directory the scanner refuses is still a
    /// perfectly valid account whose Keychain item holds real credentials, and
    /// `appliedKey` is empty until the launch-time apply lands, which is an
    /// honest answer about scanning and a wrong one about identity.
    ///
    /// Standardized with the same `standardizingPath` `expand` uses, because
    /// the Keychain service the core derives is the SHA-256 of this exact
    /// string: `/x/.claude-work/` and `/x/.claude-work` would read different
    /// items, and only one of them exists.
    static func configDirsPayloadJSON(_ configDirs: [String]) -> String {
        var seen = Set<String>()
        let standardized = configDirs
            .map { ($0 as NSString).standardizingPath }
            .filter { seen.insert($0).inserted }
        let data = (try? JSONEncoder().encode(standardized)) ?? Data("[]".utf8)
        return String(data: data, encoding: .utf8) ?? "[]"
    }

    /// Which of `paths` are missing right now, resolved off the calling actor.
    ///
    /// Settings renders a warning icon per row, and doing that from `isMissing`
    /// during view construction meant a `stat` per row on the main actor, on
    /// every render — the same stall as the setter, on the same kind of path,
    /// for a feature whose whole point is tolerating roots that are
    /// temporarily unreachable.
    ///
    /// Shares `applyQueue` with `apply`, so a probe issued after a save
    /// observes the registry that save produced rather than racing it.
    static func missingRoots(
        in paths: [String],
        then completion: @escaping @Sendable @MainActor (Set<String>) -> Void
    ) {
        applyQueue.async {
            let missing = Set(paths.filter(isMissing))
            Task { @MainActor in completion(missing) }
        }
    }

    /// Which scan roots the engine is running — the registry it ACCEPTED, not
    /// the list the user configured, and not two values that have to be kept
    /// in step.
    ///
    /// One persisted string. `apply` deliberately keeps the setter off the
    /// calling actor (the core probes each path with `read_dir`, and an
    /// unmounted volume can stall that — the case this feature exists to
    /// tolerate), so `load()` names the new roots while the engine still scans
    /// the old ones. And a call that SUCCEEDS can still refuse part of its
    /// input: `rejected` names paths Rust left out of the registry, so
    /// recording the request would claim roots no scan included. `unreadable`
    /// is the opposite case and stays — those ARE registered and retried.
    ///
    /// Persisted rather than held in memory, because the value a RESTORE needs
    /// is the registry the file on disk was written under, and that restore
    /// runs before this launch's apply has landed. Persisting one value answers
    /// both questions with the same number instead of an in-memory copy for
    /// stamping and a stored copy for validating.
    ///
    /// It moves the moment an apply succeeds while the snapshot is only
    /// rewritten by the next graph commit, so a root change followed by a quit
    /// leaves this ahead of the file and the file is rejected. That direction
    /// is the safe one: a false reject costs one cold start, a false accept
    /// shows the wrong totals.
    static let appliedKey = "tokenbar.claude.extraRootsApplied"

    /// Overridable so a self-test can move the roots DURING a fetch — the
    /// property under test — without writing the real defaults key.
    nonisolated(unsafe) static var appliedProvider: @Sendable () -> String = {
        UserDefaults.standard.string(forKey: appliedKey) ?? payloadJSON([])
    }

    static var appliedPayloadJSON: String { appliedProvider() }

    /// Record what the setter actually installed. A failed call leaves the
    /// registry holding whatever it held, so the stored value must not move.
    static func recordApplied(_ requestedJSON: String, result: ExtraScanPathsResult?) {
        _ = recordAppliedAndReportChange(requestedJSON, result: result)
    }

    /// Record, and report whether the installed registry actually MOVED.
    ///
    /// The distinction is the whole cost question. `apply` runs at launch and
    /// on every Settings save, not only when the list changes, so "an apply
    /// ran" has never implied "something changed" — yet the cache drop and the
    /// generation bump were both unconditional. Every launch therefore dropped
    /// every scan-derived cache and advanced the number the views key their
    /// reloads on, and a popover already open when that landed paid a forced,
    /// cache-bypassing rescan for a registry identical to the one already
    /// installed.
    @discardableResult
    static func recordAppliedAndReportChange(
        _ requestedJSON: String, result: ExtraScanPathsResult?
    ) -> Bool {
        guard let result else { return false }
        let json = registeredJSON(requestedJSON, rejected: result.rejected)
        guard json != appliedPayloadJSON else { return false }
        UserDefaults.standard.set(json, forKey: appliedKey)
        return true
    }

    /// `requestedJSON` minus the paths the setter refused, in the same shape.
    ///
    /// Returns the input untouched when nothing was refused, so the common case
    /// is byte-identical to `payloadJSON(load())` and no comparison drifts on
    /// re-encoding. A payload this cannot parse is also returned untouched: it
    /// came from `payloadJSON` one line earlier, so failing to parse it is
    /// impossible in practice, and guessing would be worse than recording the
    /// request.
    static func registeredJSON(_ requestedJSON: String, rejected: [ScanPathNote]) -> String {
        let refused = Set(rejected.map(\.path))
        guard !refused.isEmpty,
              let data = requestedJSON.data(using: .utf8),
              let object = try? JSONDecoder().decode([String: [String]].self, from: data)
        else { return requestedJSON }
        let kept = object.mapValues { paths in paths.filter { !refused.contains($0) } }
        // Sorted keys keep a two-client payload byte-stable after a partial
        // reject. Without it, re-encoding could emit `pi` before `claude` while
        // the next apply emits the opposite order, and the applied-payload
        // comparison would treat an unchanged registry as a root move.
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        guard let encoded = try? encoder.encode(kept),
              let text = String(data: encoded, encoding: .utf8)
        else { return requestedJSON }
        return text
    }

    /// Test seam only: back to the pre-apply state.
    static func resetAppliedForTesting() {
        appliedProvider = { UserDefaults.standard.string(forKey: appliedKey) ?? payloadJSON([]) }
        UserDefaults.standard.removeObject(forKey: appliedKey)
    }

    /// The account registry's own applied marker — the same idea as
    /// `appliedKey` above, for the OTHER registry `install` writes.
    ///
    /// It has to be a separate persisted value, not a reuse of `appliedKey` or
    /// of `lastApplied` below. `appliedKey` answers a question about scan
    /// roots; this one answers "which accounts is the quota side fetching",
    /// and the two payloads are built from the same configured list but are
    /// not the same string — a directory the scanner refuses is still absent
    /// from `appliedPayloadJSON` while it is present here. And `lastApplied`
    /// is in-memory and resets to nil every launch, which is exactly the case
    /// that needed catching: at launch there is no PREVIOUS apply for
    /// `lastApplied` to differ from, so a comparison against it always reports
    /// a change, on every process start, whether or not the account list
    /// actually differs from what a previous process already installed.
    static let appliedConfigDirsKey = "tokenbar.claude.extraRootsAppliedConfigDirs"

    /// Overridable for the same reason `appliedProvider` is.
    nonisolated(unsafe) static var appliedConfigDirsProvider: @Sendable () -> String = {
        UserDefaults.standard.string(forKey: appliedConfigDirsKey) ?? configDirsPayloadJSON([])
    }

    static var appliedConfigDirsJSON: String { appliedConfigDirsProvider() }

    /// Record, and report whether the account registry actually moved.
    ///
    /// No `result` parameter, unlike the scan side's twin: `setClaudeConfigDirs`
    /// returns nothing for `install` to check, so there is no partial-failure
    /// case to gate persistence on — the list is either installed or it is not
    /// attempted, and `install` always attempts it.
    @discardableResult
    static func recordAppliedConfigDirsAndReportChange(_ configDirsJSON: String) -> Bool {
        guard configDirsJSON != appliedConfigDirsJSON else { return false }
        UserDefaults.standard.set(configDirsJSON, forKey: appliedConfigDirsKey)
        return true
    }

    /// Test seam only: back to the pre-apply state.
    static func resetAppliedConfigDirsForTesting() {
        appliedConfigDirsProvider = {
            UserDefaults.standard.string(forKey: appliedConfigDirsKey) ?? configDirsPayloadJSON([])
        }
        UserDefaults.standard.removeObject(forKey: appliedConfigDirsKey)
    }

    /// Whether THIS PROCESS has installed the account registry at least once.
    ///
    /// `appliedConfigDirsJSON` answers "does this list differ from what a
    /// PREVIOUS install recorded", which is the wrong question on a process's
    /// first install: the Rust account registry is process-memory state and
    /// starts empty regardless of what an earlier process persisted, so a
    /// relaunch with an unchanged account list installs into an EMPTY
    /// registry while the persisted marker already says "unchanged" — the
    /// comparison reports no move for a transition that is, in this process,
    /// real.
    ///
    /// The consequence was not a cosmetic staleness. `TrayAnimator` starts its
    /// quota poll immediately at launch, before `apply`'s install has
    /// necessarily finished, and its FIRST fetch can race the setter and read
    /// only the primary account. That result is applied because nothing had
    /// signalled `RegistryChange` to move the epoch it checks against — and
    /// the loop's own recovery is `RegistryChange.sleep(upTo: 300, ...)`,
    /// which needs exactly the signal this flag exists to guarantee. Without
    /// it, a missing account could stay missing from the tray gauge for the
    /// full five minutes even though the registry had held it since launch.
    @MainActor private static var didInstallConfigDirsThisProcess = false

    /// The canonical empty-account-list payload, compared against directly
    /// rather than checking the source list for emptiness — the check has to
    /// run on the same encoded string `install` already holds.
    private static let emptyConfigDirsJSON = configDirsPayloadJSON([])

    /// Test seam only: forget that this process has installed anything.
    @MainActor
    static func resetInstalledConfigDirsThisProcessForTesting() {
        didInstallConfigDirsThisProcess = false
    }

    /// Test seam only: forget the coalescing claim, or a test that applies the
    /// same list a second time silently measures the skip path.
    @MainActor
    static func resetApplyClaimForTesting() {
        lastApplied = nil
    }

    /// Wakes anything sleeping between polls when the installed account
    /// registry moves.
    ///
    /// The quota cards come from a 60-second poll loop, so adding or removing
    /// an account left the cards describing the previous set until that loop
    /// happened to come round. Keying a view's `.task` on the generation fixed
    /// it only while that view was alive — with the popover closed, or the
    /// separate Settings window in use, nothing was observing. The registry is
    /// process-wide state, so what waits on it has to be too.
    @MainActor
    enum RegistryChange {
        /// Advanced by every `signal()`. A caller records it BEFORE the work it
        /// might be interrupted during, and passes it back to `sleep` — so a
        /// change that lands while that caller was busy is not lost.
        ///
        /// The first version had no epoch and dropped a signal that arrived
        /// with nobody waiting, which its own comment described as harmless.
        /// It is not: the quota poll spends most of each cycle inside a network
        /// fetch, so removing an account almost always signalled while the loop
        /// was busy. The signal went nowhere, the in-flight fetch published a
        /// payload built from the registry as it was before the removal, and
        /// the card stayed for the full sixty seconds — the exact symptom this
        /// was written to fix.
        private(set) static var epoch = 0
        private static var waiters: [UUID: CheckedContinuation<Void, Never>] = [:]

        /// Sleeps for `seconds`, or until the registry moves, whichever first.
        ///
        /// The timeout resumes ITS OWN waiter rather than calling `signal()`.
        /// Routing it through `signal()` read as economical and made the
        /// timeout depend on the wake-up path working: a mutation that stopped
        /// `signal()` resuming anyone did not fail the test, it hung the sleep
        /// forever — a worse outcome than the sixty-second wait this exists to
        /// remove, and in the poll loop that owns the quota cards.
        ///
        /// Registration cannot lose a race with the timeout. Both run on the
        /// MainActor, and `withCheckedContinuation`'s body runs synchronously
        /// before the suspension, so the entry is in the map before the timeout
        /// task can get a turn.
        static func sleep(upTo seconds: Double, since observed: Int) async {
            // Already stale: the registry moved while the caller was working,
            // so there is nothing to wait for and the next fetch is owed now.
            guard observed == epoch else { return }
            let id = UUID()
            let timeout = Task { @MainActor in
                try? await Task.sleep(for: .seconds(seconds))
                guard !Task.isCancelled else { return }
                resume(id)
            }
            // Cancellation-aware: a plain `withCheckedContinuation` observes
            // only `resume(id)` — a registry signal or the timeout above — so
            // cancelling the poller left it parked here for the rest of the
            // minute. The popover closing is exactly that cancel, and the
            // selftest pays it 11 times (~840s of a 958s run).
            await withTaskCancellationHandler {
                await withCheckedContinuation { waiters[id] = $0 }
            } onCancel: {
                Task { @MainActor in resume(id) }
            }
            timeout.cancel()
        }

        private static func resume(_ id: UUID) {
            waiters.removeValue(forKey: id)?.resume()
        }

        /// Releases every waiter. Idempotent: a change with nobody waiting is
        /// not an error, and a second call before anyone sleeps again is a
        /// no-op rather than a queued wake-up that would fire spuriously.
        static func signal() {
            epoch &+= 1
            let pending = waiters
            waiters.removeAll()
            for (_, waiter) in pending { waiter.resume() }
        }
    }

    private static let applyQueue = DispatchQueue(
        label: "com.nyanako.tokenbar.claude-extra-roots", qos: .utility)

    /// The payloads the most recent apply claimed, and what it answered.
    ///
    /// One Settings save reaches `apply` twice: `commitClaudeExtraRoots` calls
    /// it, and the `UserDefaults` write that call makes also trips
    /// `AppDelegate`'s observer, whose gate compares against a value only the
    /// observer updates. That was harmless when an apply merely re-registered
    /// an identical list. It stopped being harmless when apply started dropping
    /// the throttled payload and waking every poller: two of those means one
    /// edit can issue two full provider rounds, each able to spend 30 seconds
    /// on a single account, against a provider that rate-limits.
    @MainActor private static var lastApplied: (
        roots: String, configDirs: String, result: ExtraScanPathsResult?
    )?

    /// Whether this apply has work to do, claiming these payloads if so.
    ///
    /// Claimed at the top rather than recorded at the bottom: both calls for
    /// one save are made from the main actor before either has finished its
    /// trip through `applyQueue`, so a claim taken at the end would still let
    /// them both through.
    ///
    /// Compared against the LAST apply, not against every apply ever made.
    /// Removing a directory and adding it back is a real change both times,
    /// and a membership test would swallow the second one — the registry would
    /// keep whatever the removal installed while Settings showed the directory
    /// present.
    /// Test seam. `apply` itself needs the FFI and a real registry, so the
    /// decision is asserted where it is made.
    @MainActor
    static func claimApplyForTesting(roots: String, configDirs: String) -> Bool {
        claimApply(roots: roots, configDirs: configDirs)
    }

    @MainActor
    private static func claimApply(roots: String, configDirs: String) -> Bool {
        if let lastApplied, lastApplied.roots == roots, lastApplied.configDirs == configDirs {
            return false
        }
        lastApplied = (roots, configDirs, nil)
        return true
    }

    /// Push the scan-root payload to the Rust registry. Call at launch and
    /// after every add/remove so the change takes effect without an app
    /// restart (D1). Failures are logged by `TBCore`, not thrown further — a
    /// bridge failure here must not block the rest of app startup or a
    /// Settings edit.
    ///
    /// `scanPayload` is required rather than defaulted: the engine's setter is
    /// full-replace, so a Claude-only payload would silently clear every other
    /// client's roots. `ExtraScanRoots.apply` is the coordinator that builds
    /// the complete payload; this type owns the install machinery (the quota
    /// registry, applied-payload marker, generation, cache invalidation, and
    /// quota wake) that every client's apply shares.
    ///
    /// **Never runs the setter on the calling actor.** The core probes every
    /// configured path with `read_dir`, and an unmounted external volume or a
    /// stalled network mount can block that syscall for a long time — which is
    /// precisely the case this feature exists to tolerate. Doing it inline
    /// would freeze launch or the Settings window in the exact scenario the
    /// keep-and-retry behavior was built for, so the probe goes to a utility
    /// queue and only the result comes back to the main actor.
    ///
    /// Reading UserDefaults stays on the caller: it is cheap, and hopping with
    /// an already-serialized payload keeps the background block from touching
    /// app state at all.
    ///
    /// Serial, not `DispatchQueue.global()`: each call replaces the whole
    /// registry, so two applies racing — Settings removing two rows in quick
    /// succession — could finish in either order on a concurrent queue and
    /// leave the core scanning a list UserDefaults no longer holds, silently,
    /// since neither side reports an error. Serial means the last save is
    /// also the last registered.
    static func apply(
        scanPayload: @escaping @Sendable @MainActor () -> String,
        then completion: (@Sendable @MainActor (ExtraScanPathsResult?) -> Void)? = nil
    ) {
        Task { @MainActor in
            let configDirs = load()
            let json = scanPayload()
            let configDirsJSON = configDirsPayloadJSON(configDirs)
            // Both applies for one save are enqueued here before either
            // reaches the queue below, and the main actor runs them in order,
            // so the second finds the payloads already claimed. It still
            // answers its caller — with the first apply's result once that
            // lands, or nil while it is still in flight, which is what the
            // Settings row would have shown anyway.
            guard claimApply(roots: json, configDirs: configDirsJSON) else {
                completion?(lastApplied?.result)
                return
            }
            install(json: json, configDirsJSON: configDirsJSON, then: completion)
        }
    }

    /// The half of `apply` that actually talks to the core, split out so the
    /// coalescing decision above is not buried in it.
    ///
    /// The two setters are parameters with the real ones as defaults, because
    /// the property that matters here is an ORDER between them and the only way
    /// to observe an order is to hold one of them still. `M3-o` passes a scan
    /// setter that blocks and asserts the quota wake has already happened.
    @MainActor
    private static func install(
        json: String,
        configDirsJSON: String,
        setConfigDirs: @escaping @Sendable (String) -> Void = {
            _ = try? TBCore.setClaudeConfigDirs(json: $0)
        },
        setScanPaths: @escaping @Sendable (String) -> ExtraScanPathsResult? = {
            try? TBCore.setExtraScanPaths(json: $0)
        },
        then completion: (@Sendable @MainActor (ExtraScanPathsResult?) -> Void)?,
        quotaSideDone: (@Sendable @MainActor () -> Void)? = nil
    ) {
        applyQueue.async {
            // The quota-card registry, from the configured list. Separate call
            // and separate registry from the scan roots below: this one decides
            // whose credential each Claude card is fetched with, and a failure
            // in either must not stop the other from being installed.
            setConfigDirs(configDirsJSON)
            // Wake the quota side HERE, between the two setters, not after
            // both.
            //
            // The two registries have two sets of consumers and only one of the
            // setters can block. `setClaudeConfigDirs` installs a list of
            // strings; `setExtraScanPaths` probes every path with `read_dir`,
            // and a stalled network or external mount can hold that for the
            // whole mount timeout — the case this feature exists to tolerate.
            // With both notifications behind both setters, the quota cards kept
            // describing the previous account set for that entire timeout,
            // waiting on a probe whose answer they do not consume.
            //
            // Only the quota side moves. `recordApplied`, the scan-derived
            // caches and the generation stay behind `setExtraScanPaths`,
            // because those ARE about scan roots: a generation bumped early
            // would restart a load that then scans the roots this call has not
            // installed yet, which is the reason it was placed after the setter
            // in the first place.
            Task { @MainActor in
                // Test seam only, and on BOTH exits: the gated return below and
                // the wake. `completion` belongs to the scan-side task, which
                // nothing orders against this one, so a test that gated on it
                // and then read `RegistryChange.epoch` raced this task's
                // resumption after `invalidate()` (#358).
                defer { quotaSideDone?() }
                // Gated the same way the scan side is, and for the same
                // reason: `install` runs at launch and on every Settings save,
                // not only when the account list changes, so an unconditional
                // wake here paid the same repeated cost `recordAppliedAndReportChange`
                // exists to remove — just on the other registry. `lastApplied`
                // above cannot catch the launch case, because it starts nil
                // every process and a comparison against nil always reports a
                // change; `appliedConfigDirsJSON` is persisted across launches,
                // which is what the comparison needs to be meaningful here.
                //
                // Recorded before the wake, so anything the wake restarts reads
                // the registry that is now installed rather than the one it
                // replaced — same reason the scan side records before its own
                // invalidation, below.
                let recordedChange = recordAppliedConfigDirsAndReportChange(configDirsJSON)
                // The persisted comparison alone is not enough: it reports no
                // change whenever THIS process's list happens to match what a
                // PREVIOUS process installed, even on this process's first
                // install into a Rust registry that starts empty regardless.
                // `TrayAnimator`'s launch-time poll races that first install
                // directly, so the first non-empty install of a process must
                // wake pollers even when the persisted marker already agrees.
                let firstNonEmptyInstall = !didInstallConfigDirsThisProcess
                    && configDirsJSON != Self.emptyConfigDirsJSON
                didInstallConfigDirsThisProcess = true
                guard recordedChange || firstNonEmptyInstall else { return }
                // Same order as below and for the same reason: a poll woken
                // before the throttle is dropped is answered from the payload
                // built for the account set that just changed.
                await AgentUsageThrottle.shared.invalidate()
                RegistryChange.signal()
            }
            let result = setScanPaths(json)
            Task { @MainActor in
                // Before the invalidation and the generation bump, so anything
                // they restart reads the registry that is now installed rather
                // than the one it replaced.
                let moved = recordAppliedAndReportChange(json, result: result)
                // Both of these exist to undo work done under the OLD roots,
                // so both are gated on the roots having actually moved.
                //
                // They were unconditional, and the launch-time `apply()` is
                // unconditional too, so every launch dropped every scan-derived
                // cache and advanced the generation the views key their reloads
                // on. A popover already open when that landed took a forced,
                // cache-bypassing rescan on top of its initial load — the
                // registry being identical to the one already installed.
                //
                // `apply` is called on a schedule and on every Settings save,
                // not only when the list changes, so "it ran" has never implied
                // "something changed". Only the comparison does.
                guard moved else {
                    completion?(result)
                    return
                }
                // The engine dropped its own caches inside the setter. These
                // are the Swift ones, which answer without asking it — see
                // `invalidateScanDerivedCaches`. Unconditional on `completion`,
                // because whether a caller wants to hear about the result says
                // nothing about whether the caches went stale.
                DashboardModel.invalidateScanDerivedCaches()
                // Bumped AFTER the setter returns, and this is what views key
                // their scan-derived reloads on — not the persisted list. The
                // list changes the moment Settings saves, which is before this
                // queue has installed anything, so a task keyed on it can
                // restart, run, and publish while the engine is still scanning
                // the old roots. A generation moved here cannot fire early by
                // construction.
                UserDefaults.standard.set(
                    UserDefaults.standard.integer(forKey: generationKey) &+ 1,
                    forKey: generationKey)
                // The throttle drop and the poll wake are NOT repeated here.
                // They belong to the quota registry, which was installed before
                // the scan probe, and they ran there; doing them again would
                // spend a second wake on an account set that has not moved
                // since — which is the cost the apply-level coalescing exists
                // to avoid, reintroduced one layer down.
                //
                // Kept so a coalesced apply can answer its caller with what
                // this one installed rather than with nothing. Guarded, because
                // a later save may have claimed different payloads while this
                // one was in the queue: overwriting that claim with these older
                // ones would let a duplicate of the newer save through.
                if lastApplied?.roots == json, lastApplied?.configDirs == configDirsJSON {
                    lastApplied = (json, configDirsJSON, result)
                }
                completion?(result)
            }
        }
    }

    /// Test seam. Drives `install` with substituted setters so the order
    /// between installing the quota registry and probing the scan roots is
    /// observable — see `install`'s doc comment.
    ///
    /// `quotaSideDone` fires when the quota-registry task has finished, woken
    /// or not. Gate any assertion about `RegistryChange.epoch` on it rather
    /// than on `completion`: the two tasks are unordered against each other.
    @MainActor
    static func installForTesting(
        json: String,
        configDirsJSON: String,
        setConfigDirs: @escaping @Sendable (String) -> Void,
        setScanPaths: @escaping @Sendable (String) -> ExtraScanPathsResult?,
        then completion: (@Sendable @MainActor (ExtraScanPathsResult?) -> Void)?,
        quotaSideDone: (@Sendable @MainActor () -> Void)? = nil
    ) {
        install(
            json: json, configDirsJSON: configDirsJSON,
            setConfigDirs: setConfigDirs, setScanPaths: setScanPaths,
            then: completion, quotaSideDone: quotaSideDone)
    }
}
