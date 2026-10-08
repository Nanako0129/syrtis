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
    /// The one-time privacy notice was answered (Continue, Turn Off, or turning
    /// the Settings toggle on, which shows the same paragraph beside it).
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
    /// turning the toggle on answers the notice (`setEnabled`).
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

    /// `<Application Support>/<bundle id>/cursor-cache` — per bundle, so a test
    /// bundle never shares the real app's data (S-5). nil without a bundle id
    /// (bare `swift run`): no sync there.
    static func syncDirectory(
        bundleID: String? = Bundle.main.bundleIdentifier,
        appSupport: URL? = FileManager.default.urls(
            for: .applicationSupportDirectory, in: .userDomainMask).first
    ) -> String? {
        guard let bundleID, !bundleID.isEmpty, let appSupport else { return nil }
        return appSupport.appendingPathComponent(bundleID, isDirectory: true)
            .appendingPathComponent("cursor-cache", isDirectory: true).path
    }

    /// `{"enabled","dir","cliTakeoverConfirmed"}` for `tb_set_cursor_sync`.
    static func configJSON(
        dir: String, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments
    ) -> String {
        let object: [String: Any] = [
            "enabled": shouldSync(defaults: defaults, arguments: arguments),
            "dir": dir,
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

        static var all: [String] {
            [title, toggle, privacy, `continue`, turnOff, lastSynced, partial, expired, notSignedIn,
             offline, error, cliPresent, syncNow, syncing, cliQuestion, useSyrtis, keepCLI, cleanupFailed]
        }
    }

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

    private var loop: Task<Void, Never>?
    /// The last config push. Each push waits for the one before it, so rapid
    /// toggles reach the core in the order they were made.
    private var configPush: Task<Void, Never>?
    private var lastRefreshedEvents: Int?
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
        dir: String? = CursorSync.syncDirectory(),
        setConfig: @escaping @Sendable (String) -> Bool? = { (try? TBCore.setCursorSync(json: $0))?.cleanupFailed },
        sync: @escaping @Sendable (Bool) -> CursorSyncStatus? = { try? TBCore.cursorSync(explicit: $0) }
    ) {
        // Test modes make no core call at all: turning off deletes files, and
        // `--demo` runs with the real bundle id.
        guard CursorSync.isUserRuntime(arguments), let dir else { return }
        let json = CursorSync.configJSON(dir: dir, defaults: defaults, arguments: arguments)
        let run = CursorSync.shouldSync(defaults: defaults, arguments: arguments)
        loop?.cancel()
        generation &+= 1
        // Off deletes the synced files, so the next completed sync must
        // refresh even when it writes the same event count again.
        // An off push retries the cleanup: hide the last failure until it reports.
        if !run { state = nil; lastSuccessMs = nil; lastRefreshedEvents = nil; cleanupFailed = false }
        let previous = configPush
        let push = Task { [weak self] in
            await previous?.value
            let failed = await Task.detached(priority: .utility) { setConfig(json) }.value
            // Only an off push's outcome sets it (the copy says sync is off; an
            // on push deletes only when the dir moves, fixed per bundle here).
            // nil is a rejected push.
            if !run, let failed { self?.cleanupFailed = failed }
        }
        configPush = push
        loop = Task { [weak self] in
            await push.value
            if refresh { Self.refreshModel() }
            guard run else { return }
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
            let result = await Task.detached(priority: .utility) { sync(explicit) }.value
            // A reconfigure (e.g. turning sync off) happened while this ran:
            // its result describes a configuration that no longer applies.
            guard started == generation, CursorSync.shouldSync(defaults: defaults, arguments: arguments)
            else { rerunPending = true; continue }
            guard let result else { state = "error"; continue }
            state = result.state
            lastSuccessMs = result.lastSuccessMs
            // A completed walk already invalidated the core's caches; the Swift
            // side only needs telling when the data changed.
            if result.state == "ok", result.events != lastRefreshedEvents {
                lastRefreshedEvents = result.events
                Self.refreshModel()
            }
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

    /// Settings toggle. Turning it on shows the privacy paragraph beside it,
    /// so it also answers the notice.
    func setEnabled(_ on: Bool, defaults: UserDefaults = .standard) {
        defaults.set(on, forKey: CursorSync.enabledKey)
        if on { defaults.set(true, forKey: CursorSync.noticeKey) }
        reconfigure(refresh: true, defaults: defaults)
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
