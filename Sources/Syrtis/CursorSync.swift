import Foundation
import TokenBarCore

/// Cursor usage synced from the signed-in Cursor desktop app: the preferences
/// Swift owns, the config it hands the core, and the schedule.
///
/// The core switch (`tb_set_cursor_sync`) is in-memory and starts OFF every
/// launch, so `UserDefaults` is the source of truth and `reconfigure` re-applies
/// it — the `GrokBotKeychainConsent` split. Nothing here is `#if DEBUG`.
enum CursorSync {
    /// Default ON (plan D3); `object(forKey:) as? Bool` so an absent key and an
    /// explicit off are different things.
    static let enabledKey = "tokenbar.cursorSync.enabled"
    /// The one-time privacy notice was answered (Continue or Turn Off on the
    /// Overview card, or Continue in the confirmation the Settings switch opens
    /// when turned on; both show the same paragraph).
    static let noticeKey = "tokenbar.cursorSync.noticeAcknowledged"
    /// D6: the user chose Syrtis's own sync over CLI Cursor files already on
    /// this Mac. Persisted per installation; "Keep tokscale CLI Data" clears it.
    static let takeoverKey = "tokenbar.cursorSync.cliTakeoverConfirmed"
    /// D4: launch, then every 30 minutes.
    static let interval: TimeInterval = 30 * 60

    static func enabled(defaults: UserDefaults = .standard) -> Bool {
        defaults.object(forKey: enabledKey) as? Bool ?? true
    }

    static func noticeAcknowledged(defaults: UserDefaults = .standard) -> Bool {
        defaults.bool(forKey: noticeKey)
    }

    /// What the Settings toggle shows: on only when sync can actually run.
    /// Before the notice is answered nothing syncs, so the default-on
    /// preference alone must not read as "on" with every control inert;
    /// turning the toggle on asks for confirmation first
    /// (`CursorSyncController.switchChanged`), and confirming answers the notice.
    static func toggleShowsOn(enabled: Bool, acknowledged: Bool) -> Bool {
        enabled && acknowledged
    }

    static func takeoverConfirmed(defaults: UserDefaults = .standard) -> Bool {
        defaults.bool(forKey: takeoverKey)
    }

    static func isUserRuntime(_ arguments: [String] = CommandLine.arguments) -> Bool {
        !BuildIdentity.isNonUserRuntime(arguments)
    }

    /// The only gate for turning the core switch on or sending anything:
    /// a real user session (S-3), the preference, and the notice (D3). Both
    /// `configJSON` and `CursorSyncController.runSync` go through it, so no
    /// sync can precede the notice and no test mode can enable it.
    static func shouldSync(
        defaults: UserDefaults = .standard, arguments: [String] = CommandLine.arguments
    ) -> Bool {
        isUserRuntime(arguments) && enabled(defaults: defaults) && noticeAcknowledged(defaults: defaults)
    }

    /// Added Cursor profiles (`--user-data-dir` roots), each `{"id","root"}`
    /// with a random uppercase UUID. Only the folder path and that id are
    /// stored — never an account id, email or label. CMP-1 has no UI writing
    /// it; the default profile is implicit and never listed.
    static let profilesKey = "tokenbar.cursorSync.profiles"

    struct Profile: Equatable {
        let id: String
        let root: String
    }

    static func profiles(defaults: UserDefaults = .standard) -> [Profile] {
        (defaults.array(forKey: profilesKey) as? [[String: String]] ?? []).compactMap { entry in
            guard let id = entry["id"], let root = entry["root"] else { return nil }
            return Profile(id: id, root: root)
        }
    }

    /// `{"enabled","bundleId","profiles":[{"id","root"}],"cliTakeoverConfirmed"}`
    /// for `tb_set_cursor_sync`. The core derives every directory from the
    /// bundle id (per bundle, S-5) and every database path from a root; no
    /// directory is passed (CMP-520 B2).
    static func configJSON(
        bundleID: String, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments
    ) -> String {
        let object: [String: Any] = [
            "enabled": shouldSync(defaults: defaults, arguments: arguments),
            "bundleId": bundleID,
            "profiles": profiles(defaults: defaults).map { ["id": $0.id, "root": $0.root] },
            "cliTakeoverConfirmed": takeoverConfirmed(defaults: defaults),
        ]
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data()
        return String(decoding: data, as: UTF8.self)
    }

    /// Cursor desktop has run on this Mac. A file-existence check only: the
    /// notice is pointless to someone without Cursor, and nothing is read.
    static func cursorAppPresent(
        home: URL = FileManager.default.homeDirectoryForCurrentUser
    ) -> Bool {
        FileManager.default.fileExists(atPath: home.appendingPathComponent(
            "Library/Application Support/Cursor/User/globalStorage/state.vscdb").path)
    }

    static func noticeVisible(
        defaults: UserDefaults = .standard, arguments: [String] = CommandLine.arguments,
        cursorPresent: Bool = cursorAppPresent()
    ) -> Bool {
        isUserRuntime(arguments) && enabled(defaults: defaults)
            && !noticeAcknowledged(defaults: defaults) && cursorPresent
    }

    /// Approved copy (`.agent-local/plans/cursor-c3-copy.md`); the English text
    /// is the catalog key. The privacy sentence must stay true to C2's write
    /// whitelist (date, model, kind, token counts, cost fields, conversationId).
    enum Copy {
        static let title = "Cursor usage sync"
        static let toggle = "Sync Cursor usage from the Cursor app"
        static let privacy = "To show your Cursor usage, Syrtis reads the login of the Cursor app on this Mac and sends it only to Cursor's usage service (cursor.com) to download your usage. Syrtis stores the date, model, token counts, cost and conversation ID of each request on this Mac; the login itself is never saved or logged. Turning this off deletes the downloaded usage."
        static let `continue` = "Continue"
        static let cancel = "Cancel"
        static let turnOff = "Turn Off"
        static let lastSynced = "Last synced %@"
        static let partial = "Only part of your usage was downloaded. Syrtis will try again."
        static let expired = "Your Cursor login has expired. Open Cursor to refresh it."
        static let notSignedIn = "Sign in to the Cursor app to sync your usage."
        static let offline = "Can't reach Cursor. Syrtis will try again."
        static let error = "Cursor usage couldn't be synced. Syrtis will try again later."
        static let cliPresent = "Showing Cursor usage from tokscale CLI."
        static let syncNow = "Sync Now"
        static let syncing = "Syncing…"
        static let cliQuestion = "You also have Cursor usage from tokscale CLI on this Mac. Use Syrtis's own sync instead? Choose this only if it's the same Cursor account, or that account's usage will no longer be shown."
        static let useSyrtis = "Use Syrtis Sync"
        static let keepCLI = "Keep tokscale CLI Data"
        static let cleanupFailed = "Cursor sync is off, but some downloaded usage couldn't be deleted. Restart Syrtis to retry."
        /// The core refused the settings (`tb_set_cursor_sync` error): nothing syncs.
        static let configRejected = "Cursor sync couldn't be set up, so nothing is being synced."

        static var all: [String] {
            [title, toggle, privacy, `continue`, cancel, turnOff, lastSynced, partial, expired, notSignedIn,
             offline, error, cliPresent, syncNow, syncing, cliQuestion, useSyrtis, keepCLI, cleanupFailed,
             configRejected]
        }
    }

    /// Swift-side state: the last on push was refused by the core.
    static let configRejectedState = "configRejected"

    /// Status line for a core state. nil where there is nothing to say
    /// (`disabled`, not yet synced, or `ok` with no recorded time).
    static func statusLine(state: String?, lastSuccessMs: Int64?, now: Date = Date()) -> String? {
        switch state {
        case "ok":
            guard let lastSuccessMs else { return nil }
            let formatter = RelativeDateTimeFormatter()
            formatter.unitsStyle = .full
            formatter.locale = Locale(identifier: Bundle.main.preferredLocalizations.first ?? "en")
            let when = formatter.localizedString(
                for: Date(timeIntervalSince1970: Double(lastSuccessMs) / 1000), relativeTo: now)
            return Copy.lastSynced.localized(when)
        case "partial": return Copy.partial.localized
        case "expired": return Copy.expired.localized
        case "notSignedIn": return Copy.notSignedIn.localized
        case "offline": return Copy.offline.localized
        case "error": return Copy.error.localized
        case "cliPresent": return Copy.cliPresent.localized
        case configRejectedState: return Copy.configRejected.localized
        default: return nil
        }
    }
}

@MainActor
final class CursorSyncController: ObservableObject {
    static let shared = CursorSyncController()

    @Published private(set) var state: String?
    @Published private(set) var lastSuccessMs: Int64?
    @Published private(set) var syncing = false
    /// The last off push could not delete every downloaded file. Kept while
    /// sync is on (the copy says sync is off, so Settings hides it then);
    /// reset when the next off push starts and set from its outcome. Not
    /// persisted: the launch push of an off preference runs the cleanup again.
    @Published private(set) var cleanupFailed = false
    /// The Settings switch was turned on and its confirmation is open. It gates
    /// that switch only: `confirmEnable()` enables nothing unless this is set.
    /// The Overview notice answers through `answerNotice`, a separate path.
    @Published private(set) var confirmPending = false

    private var loop: Task<Void, Never>?
    /// The last config push. Each push waits for the one before it, so rapid
    /// toggles reach the core in the order they were made.
    private var configPush: Task<Void, Never>?
    /// Bumped by every `reconfigure`; a sync result that comes back after a
    /// newer configuration is discarded, so a late result cannot undo an off.
    private var generation = 0
    /// Set when a sync's result was discarded as stale; the sync reruns.
    private var rerunPending = false

    /// Push the stored preferences into the core, then (re)start the schedule
    /// if sync is allowed. Called at launch and after every preference change.
    /// `refresh`: the change can alter what the dashboard shows (a toggle or a
    /// D6 answer), so supersede the Swift caches; launch passes false, as
    /// `ClaudeExtraRoots` does, so a launch does not force a rescan.
    func reconfigure(
        refresh: Bool, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        bundleID: String? = Bundle.main.bundleIdentifier,
        setConfig: @escaping @Sendable (String) -> Bool? = { (try? TBCore.setCursorSync(json: $0))?.cleanupFailed },
        sync: @escaping @Sendable (Bool) -> CursorSyncStatus? = { try? TBCore.cursorSync(explicit: $0) }
    ) {
        // Test modes make no core call at all: turning off deletes files, and
        // `--demo` runs with the real bundle id. No bundle id (bare `swift
        // run`): no sync there.
        guard CursorSync.isUserRuntime(arguments), let bundleID, !bundleID.isEmpty else { return }
        let json = CursorSync.configJSON(bundleID: bundleID, defaults: defaults, arguments: arguments)
        let run = CursorSync.shouldSync(defaults: defaults, arguments: arguments)
        loop?.cancel()
        generation &+= 1
        // An off push retries the cleanup: hide the last failure until it reports.
        if !run { state = nil; lastSuccessMs = nil; cleanupFailed = false }
        let previous = configPush
        let push = Task { [weak self] in
            await previous?.value
            let failed = await Task.detached(priority: .utility) { setConfig(json) }.value
            // nil is a rejected push. A rejected on push is shown (the core
            // kept its previous config, so sync would silently not run) and
            // stops the schedule; the next accepted on push clears it.
            if run, let self {
                if failed == nil {
                    self.state = CursorSync.configRejectedState
                } else if self.state == CursorSync.configRejectedState {
                    self.state = nil
                }
            }
            // Only an off push's outcome sets it (the copy says sync is off;
            // an on push deletes only orphaned profile dirs).
            if !run, let failed { self?.cleanupFailed = failed }
        }
        configPush = push
        loop = Task { [weak self] in
            await push.value
            if refresh { Self.refreshModel() }
            guard run, self?.state != CursorSync.configRejectedState else { return }
            while !Task.isCancelled {
                await self?.runSync(explicit: false, defaults: defaults, arguments: arguments, sync: sync)
                try? await Task.sleep(for: .seconds(CursorSync.interval))
            }
        }
    }

    /// One sync, off the main thread. Single-flight here as well as in the core:
    /// a request while a sync runs is dropped, since that sync's result is
    /// current. A reconfigure during a sync makes its result stale; that result
    /// is discarded and the sync reruns, so the new settings get a fresh result.
    func runSync(
        explicit: Bool, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        sync: @escaping @Sendable (Bool) -> CursorSyncStatus? = { try? TBCore.cursorSync(explicit: $0) }
    ) async {
        guard CursorSync.shouldSync(defaults: defaults, arguments: arguments) else { return }
        if syncing { return }
        syncing = true
        defer { syncing = false }
        repeat {
            rerunPending = false
            // Read the generation BEFORE waiting: a settings change during the
            // wait makes this pass stale (discarded and rerun below).
            let started = generation
            // Sync against the newest configuration the core has been given.
            await configPush?.value
            // Turned off (or reconfigured) during that wait: send nothing.
            guard started == generation, CursorSync.shouldSync(defaults: defaults, arguments: arguments)
            else { rerunPending = true; continue }
            // The core refused the newest settings: there is nothing to sync.
            if state == CursorSync.configRejectedState { break }
            let result = await Task.detached(priority: .utility) { sync(explicit) }.value
            // A reconfigure (e.g. turning sync off) happened while this ran:
            // its result describes a configuration that no longer applies.
            guard started == generation, CursorSync.shouldSync(defaults: defaults, arguments: arguments)
            else { rerunPending = true; continue }
            guard let result else { state = "error"; continue }
            state = result.state
            lastSuccessMs = result.lastSuccessMs
            // A round that wrote a file already invalidated the core's
            // caches; refresh the views on `changed`, not on the headline (one
            // profile can write while another fails) and not on the event
            // count (another profile can write the same count). Every
            // successful walk rewrites its file, so this reloads the views
            // after each successful sync, at most every 30 minutes.
            if result.changed { Self.refreshModel() }
        } while rerunPending && CursorSync.shouldSync(defaults: defaults, arguments: arguments)
    }

    /// The model's normal refresh: drop Swift scan caches and bump the
    /// generation the views reload on (the `ClaudeExtraRoots` route).
    static func refreshModel() {
        DashboardModel.invalidateScanDerivedCaches()
        let defaults = UserDefaults.standard
        defaults.set(defaults.integer(forKey: ClaudeExtraRoots.generationKey) &+ 1,
                     forKey: ClaudeExtraRoots.generationKey)
        ClaudeExtraRoots.RegistryChange.signal()
    }

    // MARK: - Actions behind the controls

    /// Settings switch. Off applies at once. On enables nothing: it sets
    /// `confirmPending`, which opens the confirmation carrying the privacy
    /// paragraph. Only `confirmEnable()` enables.
    func switchChanged(
        to on: Bool, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments
    ) {
        if on {
            confirmPending = true
        } else {
            confirmPending = false
            setEnabled(false, defaults: defaults, arguments: arguments)
        }
    }

    /// Continue in the confirmation: enables and answers the notice, because
    /// the privacy paragraph was shown in the confirmation that led here.
    /// Does nothing unless the switch asked first.
    func confirmEnable(
        defaults: UserDefaults = .standard, arguments: [String] = CommandLine.arguments
    ) {
        guard confirmPending else { return }
        confirmPending = false
        setEnabled(true, defaults: defaults, arguments: arguments)
    }

    /// Cancel, Escape, clicking away or leaving the page: enables nothing.
    func cancelEnable() { confirmPending = false }

    private func setEnabled(
        _ on: Bool, defaults: UserDefaults, arguments: [String]
    ) {
        defaults.set(on, forKey: CursorSync.enabledKey)
        if on { defaults.set(true, forKey: CursorSync.noticeKey) }
        reconfigure(refresh: true, defaults: defaults, arguments: arguments)
    }

    /// Notice: Continue (`enable`) or Turn Off.
    func answerNotice(continuing: Bool, defaults: UserDefaults = .standard) {
        defaults.set(true, forKey: CursorSync.noticeKey)
        if !continuing { defaults.set(false, forKey: CursorSync.enabledKey) }
        reconfigure(refresh: false, defaults: defaults)
    }

    /// D6: "Use Syrtis Sync" (true) or "Keep tokscale CLI Data" (false).
    func setTakeoverConfirmed(_ confirmed: Bool, defaults: UserDefaults = .standard) {
        defaults.set(confirmed, forKey: CursorSync.takeoverKey)
        reconfigure(refresh: true, defaults: defaults)
    }
}
