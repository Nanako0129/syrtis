import Foundation
import TokenBarCore

/// The single coordinator that builds one `tb_set_extra_scan_paths` payload
/// from every client's configured roots. The setter is full-replace, so a
/// payload for one client alone would silently clear the others; Claude's
/// expanded roots and the additional Pi and DSH roots therefore always
/// travel together.
///
/// The engine's `pi` lane scans `*.jsonl` under each root with the same parser
/// it uses for `~/.pi/agent/sessions`. A usage-only export (session header plus
/// assistant records with usage/model) and a full Pi session are both accepted
/// there; no second parser exists. Roots merge into the single Pi total, and
/// the Pi lane's cross-file `responseId` dedup means an export and a real
/// session for the same run count once.
enum ExtraScanRoots {
    /// Additional Pi directories, persisted exactly as configured.
    ///
    /// No default and no discovery: an empty list means Syrtis scans only the
    /// normal `~/.pi/agent/sessions` tree. These are user-chosen roots, not a
    /// producer-specific convention.
    enum Pi {
        static let storageKey = "tokenbar.pi.extraRoots"

        static func load() -> [String] {
            guard let raw = UserDefaults.standard.string(forKey: storageKey),
                let data = raw.data(using: .utf8),
                let paths = try? JSONDecoder().decode([String].self, from: data)
            else { return [] }
            return paths
        }

        static func save(_ paths: [String]) {
            let data = (try? JSONEncoder().encode(standardizedUnique(paths))) ?? Data("[]".utf8)
            UserDefaults.standard.set(
                String(data: data, encoding: .utf8) ?? "[]", forKey: storageKey)
        }

        /// Kept in the list when absent: an unmounted volume or a directory a
        /// runner has not created yet can become readable later, and the scan
        /// registry already retries it. Settings shows the warning; dropping
        /// the row would lose the user's configuration instead.
        static func isMissing(_ path: String) -> Bool {
            ClaudeExtraRoots.isMissing(path)
        }

        /// Path-only, so it never blocks the main actor on a stalled mount.
        /// Unlike a Claude root, which `expand` narrows to two sub-folders, a
        /// Pi root is walked recursively as-is, so home, the filesystem root
        /// and every ancestor of home are refused. A non-directory is refused
        /// by the Rust setter (`classify`) and reported in `rejected`; a
        /// missing path is kept and reported as missing for a later scan.
        static func isRejectedRoot(_ path: String) -> Bool {
            let standardized = standardizedPath(path)
            let home = FileManager.default.homeDirectoryForCurrentUser.path
            return standardized.isEmpty || standardized == "/" || standardized == home
                || home.hasPrefix(standardized + "/")
        }

        /// The refusal reason code for adding `path` after `existing`, or nil.
        /// Path-only like `isRejectedRoot`. Codes share `AccountsCopy.refusalSentence`
        /// with the Claude list: `empty_path`, `filesystem_root`, `duplicate`,
        /// and `home_or_ancestor` for home or a folder containing it.
        static func refusal(adding path: String, to existing: [String]) -> String? {
            let standardized = standardizedPath(path)
            if standardized.isEmpty { return "empty_path" }
            if standardized == "/" { return "filesystem_root" }
            if isRejectedRoot(path) { return "home_or_ancestor" }
            if existing.map(standardizedPath).contains(standardized) { return "duplicate" }
            return nil
        }

        /// Probe paths off the main actor on the setter's serial queue. A probe
        /// issued right after a save can run before that save's install, which
        /// `apply` enqueues from a later main-actor task.
        static func missingRoots(
            in paths: [String],
            then completion: @escaping @Sendable @MainActor (Set<String>) -> Void
        ) {
            ClaudeExtraRoots.missingRoots(in: paths, then: completion)
        }
    }

    /// Additional DeepSeek Harness session folders (`<DSH_HOME>/sessions`),
    /// persisted exactly as configured. The engine's `dsh` lane walks each root
    /// for `session*.jsonl.zstd` like `~/.dsh/sessions`, and its cross-file
    /// dedup key carries no session id or root, so one session reachable under
    /// two roots counts once. Same path rules as Pi: recursive walk, so the
    /// same too-broad roots are refused.
    enum Dsh {
        static let storageKey = "tokenbar.dsh.extraRoots"

        /// `defaults` is a parameter only so SelfTest can use a scratch suite
        /// instead of the app's real preferences.
        static func load(defaults: UserDefaults = .standard) -> [String] {
            guard let raw = defaults.string(forKey: storageKey),
                let data = raw.data(using: .utf8),
                let paths = try? JSONDecoder().decode([String].self, from: data)
            else { return [] }
            return paths
        }

        static func save(_ paths: [String], defaults: UserDefaults = .standard) {
            let data = (try? JSONEncoder().encode(standardizedUnique(paths))) ?? Data("[]".utf8)
            defaults.set(String(data: data, encoding: .utf8) ?? "[]", forKey: storageKey)
        }

        static func isRejectedRoot(_ path: String) -> Bool { Pi.isRejectedRoot(path) }
        static func refusal(adding path: String, to existing: [String]) -> String? {
            Pi.refusal(adding: path, to: existing)
        }
        static func missingRoots(
            in paths: [String],
            then completion: @escaping @Sendable @MainActor (Set<String>) -> Void
        ) {
            Pi.missingRoots(in: paths, then: completion)
        }
    }

    /// What an Add click on any of the three lists produces: the list, and the
    /// fixed sentence to show under the button when the folder was refused.
    /// Decided here, off the view, so a test can observe the sentence.
    struct AddOutcome: Equatable {
        let list: [String]
        let refusalLine: String?
    }

    /// `refusal` is the list's own rule (`ClaudeExtraRoots.refusal(adding:to:)`,
    /// `Pi.refusal`, `Dsh.refusal`); a refused folder is not added.
    static func add(
        _ path: String, to list: [String], refusal: (String, [String]) -> String?
    ) -> AddOutcome {
        if let code = refusal(path, list) {
            return AddOutcome(list: list, refusalLine: AccountsCopy.refusalSentence(code))
        }
        return AddOutcome(list: list + [path], refusalLine: nil)
    }

    /// The complete payload for all clients.
    ///
    /// With no Pi and no DSH roots this returns `ClaudeExtraRoots.payloadJSON`
    /// unchanged byte-for-byte. That is not cosmetic: `appliedKey` stores the last
    /// accepted payload, and `install` drops the scan caches and advances the
    /// generation only when it moved. Re-encoding the same registry
    /// differently on the first launch after an upgrade would force a
    /// cache-bypassing rescan and reject the restart snapshot for a root set
    /// that never changed.
    ///
    /// The multi-key form is assembled from separately encoded arrays rather
    /// than a dictionary so its byte order is stable across launches (always
    /// `claude`, then `pi`, then `dsh`). The engine reads a full object and the
    /// setter is full-replace, so `claude` is always present, while `pi` and
    /// `dsh` appear only when non-empty: an absent key already means "no roots
    /// for that client". That also keeps every pre-DSH payload byte-identical.
    ///
    /// `claudeAccepted`: the directories the core already accepted for these
    /// `claudeConfigDirs`, when the caller has asked the core once and
    /// reuses the answer (`ClaudeExtraRoots.applyPayloads`); nil asks here.
    static func payloadJSON(
        claudeConfigDirs: [String], claudeAccepted: [String]? = nil, piRoots: [String], dshRoots: [String]
    ) -> String {
        let piPaths = standardizedUnique(piRoots)
        let dshPaths = standardizedUnique(dshRoots)
        let claudeRoots = ClaudeExtraRoots.Classified(
            refused: [:], accepted: claudeAccepted ?? ClaudeExtraRoots.accepted(claudeConfigDirs)
        ).expanded
        guard !piPaths.isEmpty || !dshPaths.isEmpty else {
            return ClaudeExtraRoots.payloadJSON(expanded: claudeRoots)
        }
        var json = "{\"claude\":\(encode(claudeRoots))"
        if !piPaths.isEmpty { json += ",\"pi\":\(encode(piPaths))" }
        if !dshPaths.isEmpty { json += ",\"dsh\":\(encode(dshPaths))" }
        return json + "}"
    }

    /// One client's share of a merged apply result, for the per-section hints
    /// in Settings.
    ///
    /// Both sections apply ONE merged payload, so the raw result mixes both
    /// clients' notes and `registeredCount` spans the whole registry — the Pi
    /// section showing it would blame an unreadable Claude root on the Pi
    /// list. Notes are filtered by client, and the per-client registered
    /// count is derived as requested minus rejected: a rejected path never
    /// enters the registry, while an unreadable one DOES and is retried.
    struct ClientHint {
        let unreadable: Int
        let rejected: Int
        let registered: Int
    }

    static func clientHint(
        _ result: ExtraScanPathsResult?, client: String, requested: Int
    ) -> ClientHint? {
        guard let result else { return nil }
        let rejected = result.rejected.filter { $0.client == client }.count
        return ClientHint(
            unreadable: result.unreadable.filter { $0.client == client }.count,
            rejected: rejected,
            registered: max(requested - rejected, 0))
    }

    /// Apply the merged registry at launch and after every edit. The install
    /// machinery (quota registry, applied marker, generation, cache
    /// invalidation, and quota wake) stays in `ClaudeExtraRoots`, which is
    /// where every client's apply has always run.
    static func apply(
        then completion: (@Sendable @MainActor (ExtraScanPathsResult?) -> Void)? = nil
    ) {
        ClaudeExtraRoots.apply(scanPayload: { currentPayload(claudeAccepted: $0) }, then: completion)
    }

    /// The payload `apply` hands to the core, built from the persisted lists.
    /// A separate function so SelfTest observes the production path;
    /// `dshDefaults` is only for a scratch suite.
    static func currentPayload(
        dshDefaults: UserDefaults = .standard, claudeAccepted: [String]? = nil
    ) -> String {
        payloadJSON(
            claudeConfigDirs: ClaudeExtraRoots.load(), claudeAccepted: claudeAccepted,
            piRoots: Pi.load(), dshRoots: Dsh.load(defaults: dshDefaults))
    }

    private static func standardizedPath(_ path: String) -> String {
        (path as NSString).standardizingPath
    }

    private static func standardizedUnique(_ paths: [String]) -> [String] {
        var seen = Set<String>()
        return paths.map(standardizedPath).filter { seen.insert($0).inserted }
    }

    private static func encode(_ paths: [String]) -> String {
        let data = (try? JSONEncoder().encode(paths)) ?? Data("[]".utf8)
        return String(data: data, encoding: .utf8) ?? "[]"
    }
}
