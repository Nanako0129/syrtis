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
    /// stored — never an account id, email or label. Written by the Accounts
    /// page (`CursorSyncController.addProfile` / `removeProfile`); the default
    /// profile is implicit and never listed.
    static let profilesKey = "tokenbar.cursorSync.profiles"

    struct Profile: Equatable {
        let id: String
        let root: String
        /// The stored and wire shape, `{"id","root"}`.
        var stored: [String: String] { ["id": id, "root": root] }
    }

    /// The core's limit (`cursor_sync::MAX_PROFILES`): a longer list is
    /// rejected whole, the default profile included.
    static let maxProfiles = 32

    static func profiles(defaults: UserDefaults = .standard) -> [Profile] {
        (defaults.array(forKey: profilesKey) as? [[String: String]] ?? []).compactMap { entry in
            guard let id = entry["id"], let root = entry["root"] else { return nil }
            return Profile(id: id, root: root)
        }
    }

    static func saveProfiles(_ profiles: [Profile], defaults: UserDefaults = .standard) {
        defaults.set(profiles.map(\.stored), forKey: profilesKey)
    }

    /// One spelling per folder: symlinks resolved, case folded (APFS's
    /// default is case-insensitive).
    static func folderKey(_ path: String) -> String {
        URL(fileURLWithPath: path).resolvingSymlinksInPath().standardizedFileURL.path.lowercased()
    }

    /// Why a folder can't be added, checked before anything is saved: one bad
    /// entry would make the core reject the whole config and stop every
    /// profile's sync. nil: it can be added.
    static func addRefusal(
        root: String, existing: [Profile],
        home: URL = FileManager.default.homeDirectoryForCurrentUser
    ) -> String? {
        let standard = (root as NSString).standardizingPath
        let key = folderKey(standard)
        let cursorOwn = folderKey(home.appendingPathComponent("Library/Application Support/Cursor").path)
        if existing.count >= maxProfiles || !standard.hasPrefix("/") || standard == "/"
            || key == cursorOwn || existing.contains(where: { folderKey($0.root) == key }) {
            return Copy.cantAdd
        }
        return nil
    }

    /// What the consent surfaces show: the privacy paragraph, plus the added
    /// profiles' paragraph once any folder is stored (they are read too).
    static func consentParagraphs(defaults: UserDefaults = .standard) -> [String] {
        profiles(defaults: defaults).isEmpty ? [Copy.privacy] : [Copy.privacy, Copy.profilesPrivacy]
    }

    typealias SetConfig = @Sendable (String) -> Bool?
    typealias Sync = @Sendable (Bool) -> CursorSyncStatus?
    /// The real core calls; tests inject spies in their place.
    static let coreSetConfig: SetConfig = { (try? TBCore.setCursorSync(json: $0))?.cleanupFailed }
    static let coreSync: Sync = { try? TBCore.cursorSync(explicit: $0) }

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
            "profiles": profiles(defaults: defaults).map(\.stored),
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
        /// Accounts page, profile list (CMP-520). The default profile's row.
        static let defaultProfile = "Cursor app (default)"
        static let addProfile = "Add Cursor profile folder…"
        /// A row whose account an earlier profile already synced this round.
        static let duplicate = "Same account as another profile; counted once."
        /// Second paragraph, always shown under `privacy` (the ⓘ, and the
        /// consent surfaces via `consentParagraphs`), so "above" is that
        /// paragraph. Each sentence, checked against the code:
        /// handled as above — an added profile goes through the same walk as
        /// the default: `cursor_desktop::read_login_checked` on
        /// `<root>/User/globalStorage/state.vscdb` each round,
        /// `cursor_sync::walk` posting only to `USAGE_EVENTS_URL` (cursor.com,
        /// https-only, no redirects; added profiles never feed Grok Bot),
        /// `commit_file` writing only the parser fields into
        /// `<appDir>/cursor-profiles/<id>`, and `CursorLogin` dropped after the
        /// cookie (never stored or logged); one total, counted once —
        /// `newest_per_account`; deletes the next time Syrtis can — orphan
        /// cleanup on the next accepted on push (`set_from_json`), a failure
        /// raising `cleanupFailed`.
        static let profilesPrivacy = "Add a folder you start Cursor with using --user-data-dir to sync it too; each profile's sign-in is handled as described above. All profiles add up to one Cursor total, and an account signed in to several profiles counts once. Removing a profile deletes its downloaded usage the next time Syrtis can."

        /// Under the add button when `addRefusal` refuses a folder.
        static let cantAdd = "That folder can't be added."
        /// A row whose own reason is permanent (`permanentReasons`).
        static let cantRead = "Syrtis can't read this folder's Cursor sign-in."
        /// While on: an on push could not delete a removed profile's files.
        static let cleanupFailedOn = "Some downloaded usage couldn't be deleted. Restart Syrtis to retry."
        /// Accessibility label of a row's Remove button; `%@` is the folder name.
        static let removeFolder = "Remove %@"

        static var all: [String] {
            [title, toggle, privacy, `continue`, cancel, turnOff, lastSynced, partial, expired, notSignedIn,
             offline, error, cliPresent, syncNow, syncing, cliQuestion, useSyrtis, keepCLI, cleanupFailed,
             configRejected, defaultProfile, addProfile, duplicate, profilesPrivacy, cantAdd, cantRead,
             cleanupFailedOn, removeFolder]
        }
    }

    /// Swift-side state: the last on push was refused by the core.
    static let configRejectedState = "configRejected"

    /// Per-profile reasons a retry will not fix.
    static let permanentReasons: Set<String> = [
        "db_symlink", "db_not_file", "db_foreign_owner", "login_unreadable", "app_dir_rejected",
    ]

    /// A profile row's line: the duplicate note, the permanent-failure line,
    /// or the status line for its state.
    static func rowLine(_ status: CursorSyncStatus.Profile) -> String? {
        if status.state == "duplicate" { return Copy.duplicate.localized }
        if let reason = status.reason, permanentReasons.contains(reason) { return Copy.cantRead.localized }
        return statusLine(state: status.state, lastSuccessMs: status.lastSuccessMs)
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
    /// The last sync's outcome per profile id (`default` or a UUID), so a row
    /// shows its own error even when the headline says something else.
    @Published private(set) var profileStates: [String: CursorSyncStatus.Profile] = [:]

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
        setConfig: @escaping CursorSync.SetConfig = CursorSync.coreSetConfig,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
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
        if !run { state = nil; lastSuccessMs = nil; cleanupFailed = false; profileStates = [:] }
        let push = pushConfig(json, run: run, setConfig: setConfig)
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

    /// Queue one config push after the previous one. nil is a rejected push.
    /// A rejected on push is shown (the core kept its previous config, so
    /// sync would silently not run), clears the rows' stale outcomes and stops
    /// the schedule; the next accepted on push clears it. An off push's
    /// outcome sets `cleanupFailed`; an on push (which deletes removed
    /// profiles' files) only raises it.
    private func pushConfig(_ json: String, run: Bool, setConfig: @escaping CursorSync.SetConfig) -> Task<Void, Never> {
        let previous = configPush
        let push = Task { [weak self] in
            await previous?.value
            let failed = await Task.detached(priority: .utility) { setConfig(json) }.value
            guard let self else { return }
            if run {
                if failed == nil {
                    self.state = CursorSync.configRejectedState
                    self.profileStates = [:]
                } else if self.state == CursorSync.configRejectedState {
                    self.state = nil
                }
                if failed == true { self.cleanupFailed = true }
            } else if let failed {
                self.cleanupFailed = failed
            }
        }
        configPush = push
        return push
    }

    /// One sync, off the main thread. Single-flight here as well as in the core:
    /// a request while a sync runs is dropped, since that sync's result is
    /// current. A reconfigure during a sync makes its result stale; that result
    /// is discarded and the sync reruns, so the new settings get a fresh result.
    func runSync(
        explicit: Bool, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
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
            guard let result else { state = "error"; profileStates = [:]; continue }
            state = result.state
            lastSuccessMs = result.lastSuccessMs
            profileStates = Dictionary(
                (result.profiles ?? []).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
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

    /// Accounts page: add a `--user-data-dir` folder. Stores only a new
    /// uppercase UUID and the path, after `addRefusal` (returned when it
    /// refuses; nothing is saved then). While sync can't run (off, or the
    /// notice unanswered) nothing reaches the core (no config push, no sync,
    /// no database read); the stored list is pushed when sync is turned on.
    /// While on, the reconfigure syncs at once.
    @discardableResult
    func addProfile(
        root: String, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        bundleID: String? = Bundle.main.bundleIdentifier,
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        setConfig: @escaping CursorSync.SetConfig = CursorSync.coreSetConfig,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
    ) -> String? {
        var list = CursorSync.profiles(defaults: defaults)
        if let refusal = CursorSync.addRefusal(root: root, existing: list, home: home) { return refusal }
        list.append(CursorSync.Profile(id: UUID().uuidString, root: (root as NSString).standardizingPath))
        CursorSync.saveProfiles(list, defaults: defaults)
        guard profilesCanSync(defaults: defaults, arguments: arguments, bundleID: bundleID) else { return nil }
        reconfigure(refresh: true, defaults: defaults, arguments: arguments, bundleID: bundleID,
                    setConfig: setConfig, sync: sync)
        return nil
    }

    /// Accounts page Remove. While on, pushes the config without it (the
    /// core's orphan cleanup deletes its downloaded usage; a failure raises
    /// `cleanupFailed`, a rejection shows `configRejected`) and refreshes the
    /// views, but starts no sync: the schedule keeps its timing. While off it
    /// reaches no core call; its usage was deleted when sync was turned off.
    func removeProfile(
        id: String, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        bundleID: String? = Bundle.main.bundleIdentifier,
        setConfig: @escaping CursorSync.SetConfig = CursorSync.coreSetConfig
    ) {
        CursorSync.saveProfiles(CursorSync.profiles(defaults: defaults).filter { $0.id != id }, defaults: defaults)
        profileStates[id] = nil
        guard profilesCanSync(defaults: defaults, arguments: arguments, bundleID: bundleID),
            let bundleID
        else { return }
        let push = pushConfig(
            CursorSync.configJSON(bundleID: bundleID, defaults: defaults, arguments: arguments),
            run: true, setConfig: setConfig)
        Task { await push.value; Self.refreshModel() }
    }

    /// The gate for profile edits: the same one every sync passes.
    private func profilesCanSync(defaults: UserDefaults, arguments: [String], bundleID: String?) -> Bool {
        CursorSync.shouldSync(defaults: defaults, arguments: arguments) && !(bundleID ?? "").isEmpty
    }

    /// D6: "Use Syrtis Sync" (true) or "Keep tokscale CLI Data" (false).
    func setTakeoverConfirmed(_ confirmed: Bool, defaults: UserDefaults = .standard) {
        defaults.set(confirmed, forKey: CursorSync.takeoverKey)
        reconfigure(refresh: true, defaults: defaults)
    }
}
