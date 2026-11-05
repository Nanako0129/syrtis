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

    /// The folder's file identity (a final symlink followed), so another
    /// spelling of the same folder — a symlink, a firmlink such as
    /// /System/Volumes/Data, a different case — compares equal on any volume.
    /// nil for a folder that can't be read (gone, unmounted): never an
    /// identity match. When the folder being added has no identity, `cantAdd`
    /// compares standardized path strings instead; a stored or Cursor-own
    /// folder without one only matches through that path comparison.
    static func folderIdentity(_ path: String) -> NSObject? {
        (try? URL(fileURLWithPath: path).resolvingSymlinksInPath()
            .resourceValues(forKeys: [.fileResourceIdentifierKey]))?.fileResourceIdentifier as? NSObject
    }

    /// A folder that can't be added, checked before anything is saved. A 33rd
    /// profile or a relative path or "/" would make the core reject the whole
    /// config (`cursor_sync::parse_config`) and stop every profile's sync.
    /// Cursor's own folder and a folder already listed are refused only to
    /// avoid a duplicate row and a second read of the same login.
    static func cantAdd(
        root: String, existing: [Profile],
        home: URL = FileManager.default.homeDirectoryForCurrentUser
    ) -> Bool {
        let standard = (root as NSString).standardizingPath
        if existing.count >= maxProfiles || !standard.hasPrefix("/") || standard == "/" { return true }
        let cursorOwn = home.appendingPathComponent("Library/Application Support/Cursor").path
        let candidates = [cursorOwn] + existing.map(\.root)
        guard let identity = folderIdentity(standard) else {
            // Unreadable: the path string is all there is to compare.
            return candidates.contains { ($0 as NSString).standardizingPath == standard }
        }
        return candidates.contains { folderIdentity($0)?.isEqual(identity) == true }
    }

    /// What the consent surfaces show: the privacy paragraph, plus the added
    /// profiles' paragraph once any folder is stored (they are read too).
    static func consentParagraphs(profiles: [Profile]) -> [String] {
        profiles.isEmpty ? [Copy.privacy] : [Copy.privacy, Copy.profilesPrivacy]
    }

    /// What an accepted `tb_set_cursor_sync` push reports: whether the cleanup
    /// left files behind, and the profiles the core left out of the push
    /// (index in the pushed list to reason).
    struct Push: Equatable, Sendable {
        var cleanupFailed: Bool
        var rejected: [Int: String] = [:]

        /// Nothing left behind, nothing refused.
        static let clean = Push(cleanupFailed: false)
    }

    /// `nil` = the core refused the whole push.
    typealias SetConfig = @Sendable (String) -> Push?
    typealias Sync = @Sendable (Bool) -> CursorSyncStatus?
    /// The real core calls; tests inject spies in their place.
    static let coreSetConfig: SetConfig = { json in
        guard let result = try? TBCore.setCursorSync(json: json) else { return nil }
        return Push(
            cleanupFailed: result.cleanupFailed,
            rejected: Dictionary(
                (result.rejectedProfiles ?? []).map { ($0.index, $0.reason) },
                uniquingKeysWith: { first, _ in first }))
    }
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
        /// `error` plus the core's fixed reason code (`%@`), so a screenshot can be diagnosed.
        static let errorWithReason = "Cursor usage couldn't be synced (%@). Syrtis will try again later."
        static let cliPresent = "Showing Cursor usage from tokscale CLI."
        static let syncNow = "Sync Now"
        static let syncing = "Syncing…"
        static let cliQuestion = "You also have Cursor usage from tokscale CLI on this Mac. Use Syrtis's own sync instead? Choose this only if it's the same Cursor account, or that account's usage will no longer be shown."
        static let useSyrtis = "Use Syrtis Sync"
        static let keepCLI = "Keep tokscale CLI Data"
        static let cleanupFailed = "Cursor sync is off, but some downloaded usage couldn't be deleted. Restart Syrtis to retry."
        /// Approved copy (`multi-account-mac-copy-draft.md`, section 7). Shown
        /// while stale downloaded usage is known to remain. When each cleanup
        /// runs again:
        /// - The orphan cleanup of an on push (a removed profile's dir, an
        ///   earlier appDir) runs on the next accepted on push, after any
        ///   Cursor setting changes or at launch, and the line clears when
        ///   that push reports no failure.
        /// - An earlier appDir is retried by each accepted push only while
        ///   Syrtis keeps running; after a relaunch it is no longer known and
        ///   is not retried.
        /// - A duplicate profile's old account file is dropped in every round
        ///   in which that profile is still a duplicate, and a successful walk
        ///   drops its profile's other usage files; in any other state (not
        ///   signed in, expired, an error) nothing retries it, so the line
        ///   stays until a round shows that profile `duplicate` or `ok`
        ///   without a failure, or the profile is removed (`staleAfter`).
        static let staleCleanupFailed = "Some downloaded Cursor usage that is no longer needed couldn't be deleted. Syrtis will try again."
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
        /// `newest_per_account`; deletes the next time Syrtis can — the core's
        /// orphan cleanup runs on the next accepted on push (`set_from_json`);
        /// a failure there is not surfaced. Folder path in settings —
        /// `profilesKey`.
        static let profilesPrivacy = "Add a folder you start Cursor with using --user-data-dir to sync it too (Syrtis keeps the folder path in its settings); each profile's sign-in is handled as described above. All profiles add up to one Cursor total, and an account signed in to several profiles counts once. Removing a profile deletes its downloaded usage the next time Syrtis can."

        /// The refusal alert's text when `cantAdd` refuses a folder.
        static let cantAdd = "That folder can't be added."
        /// A row whose own reason is permanent (`permanentReasons`).
        static let cantRead = "Syrtis can't read this folder's Cursor sign-in."
        /// Accessibility label of a row's Remove button; `%@` is the row's label
        /// (`rowLabel`), the whole folder and not its last component.
        static let removeFolder = AccountsCopy.removeNamed

        /// Cursor 3.27+ encrypted sign-in (plan CURSOR-SS-M, Design 8-9). Draft
        /// copy: the disclosure is final only after the device check shows what
        /// the macOS dialog asks (plan Acceptance 4c).
        /// `keychain_consent`: a sealed sign-in and no "cursor" consent yet.
        static let keychainConsent = "Cursor stores its sign-in encrypted. Syrtis needs your permission to read it."
        /// `keychain_denied`: macOS refused (Deny in its dialog).
        static let keychainDenied = "macOS didn't let Syrtis read Cursor's encrypted sign-in."
        /// `keychain_unlock`: no key in memory, and only a round the user starts reads the Keychain.
        static let keychainUnlock = "Press Sync Now to read Cursor's encrypted sign-in."
        static let keychainAllowAccess = "Allow Keychain access…"
        static let keychainDisclosure = "Cursor 3.27 and later can store its sign-in encrypted. To read it, Syrtis needs the \"Cursor Safe Storage\" key from your Keychain. That key can decrypt everything Cursor stores encrypted; Syrtis uses it only to read the Cursor sign-in of the profiles listed here, keeps it in memory while Syrtis runs, and never saves or logs it. The sign-in is still sent only to cursor.com. Turning on Cursor sync does not grant this; macOS will ask you to allow Syrtis. If Cursor stores its sign-in unencrypted, nothing is asked."
        /// The semantic button key the Grok Bot consent already uses (English
        /// "Allow"; the bare "Allow" key is the badge, a different part of speech).
        static let allow = "consent.action.allow"

        /// Cursor Billing V2 (plan CURSOR-BV2-M, design 7). Each keeps the
        /// core's code (`%@`) on the #547 line so a screenshot can be
        /// diagnosed. Draft copy, shown to the maintainer before merge.
        /// `billing_v2_team_required`: V2 answered `missing_team_id`.
        static let billingV2TeamRequired = "Cursor's new billing asks for a team for this account, which Syrtis can't read yet (%@)."
        /// `billing_v2_unavailable`: V2 answered another `availability`.
        static let billingV2Unavailable = "Cursor's new billing didn't provide this account's usage (%@). Syrtis will try again later."
        /// `billing_v2_no_items`: no earlier usage and V2 returned none.
        static let billingV2NoItems = "Cursor's new billing has no usage for this account yet (%@). Syrtis will try again later."
        /// `billing_v2_principal_mismatch`: V2 returned another account's rows; nothing saved.
        static let billingV2PrincipalMismatch = "Cursor returned usage of another account, so Syrtis saved nothing (%@)."

        static var all: [String] {
            [title, toggle, privacy, `continue`, cancel, turnOff, lastSynced, partial, expired, notSignedIn,
             offline, error, errorWithReason, cliPresent, syncNow, syncing, cliQuestion, useSyrtis, keepCLI, cleanupFailed,
             configRejected, staleCleanupFailed, defaultProfile, addProfile, duplicate, profilesPrivacy, cantAdd, cantRead,
             removeFolder, keychainConsent, keychainDenied, keychainUnlock, keychainAllowAccess, keychainDisclosure, allow,
             billingV2TeamRequired, billingV2Unavailable, billingV2NoItems, billingV2PrincipalMismatch]
        }
    }

    /// Swift-side state: the last on push was refused by the core.
    static let configRejectedState = "configRejected"

    /// Reasons answered by the "Allow Keychain access…" button: no consent
    /// yet, or macOS refused (the stored answer was withdrawn).
    static let keychainAllowReasons: Set<String> = ["keychain_consent", "keychain_denied"]

    /// The headline or any profile asks for Keychain consent.
    static func needsKeychainAllow(reason: String?, profiles: [String: CursorSyncStatus.Profile]) -> Bool {
        anyReason(in: keychainAllowReasons, headline: reason, profiles: Array(profiles.values))
    }

    /// One of `codes` on the headline or on any profile. Shared by the Allow
    /// button and the Cursor Keychain revert, so both read a round the same way.
    static func anyReason(
        in codes: Set<String>, headline: String?, profiles: [CursorSyncStatus.Profile]
    ) -> Bool {
        ([headline] + profiles.map(\.reason)).contains { code in
            code.map { codes.contains($0) } ?? false
        }
    }

    /// Per-profile reasons a retry will not fix.
    static let permanentReasons: Set<String> = [
        "db_symlink", "db_not_file", "db_foreign_owner",
    ]

    /// The cleanup failure to show under the switch. While the switch shows
    /// off, the off push's (its copy says sync is off); while on, the stale
    /// usage that could not be deleted: an on push's orphan cleanup or a
    /// duplicate profile's old file.
    static func cleanupLine(syncOn: Bool, cleanupFailed: Bool, staleCleanupFailed: Bool) -> String? {
        if syncOn { return staleCleanupFailed ? Copy.staleCleanupFailed.localized : nil }
        return cleanupFailed ? Copy.cleanupFailed.localized : nil
    }

    /// A profile's row label in Settings: its folder with the home directory
    /// abbreviated. Also the name of its Remove button, whole, so two profiles
    /// never share one.
    static func rowLabel(_ profile: Profile) -> String {
        (profile.root as NSString).abbreviatingWithTildeInPath
    }

    /// The profiles with a stale usage file after a sync round. Those the round
    /// reports (`cleanupFailedProfiles`) are stale. One that was stale and is
    /// not reported stays stale unless this round shows its cleanup ran: the
    /// profile is `duplicate` or `ok` (a duplicate drops its old account's file
    /// and a successful walk drops the other usage files, both reporting any
    /// failure), or it is no longer configured. In any other state (not signed
    /// in, expired, an error) nothing retries the delete, so it stays.
    /// A status without `profiles` (an older core) clears nothing.
    static func staleAfter(_ stale: Set<String>, status: CursorSyncStatus) -> Set<String> {
        let failed = Set(status.cleanupFailedProfiles ?? [])
        guard let profiles = status.profiles else { return stale.union(failed) }
        let states = Dictionary(profiles.map { ($0.id, $0.state) }, uniquingKeysWith: { first, _ in first })
        let kept = stale.filter { id in
            guard let state = states[id] else { return false }  // removed
            return !["duplicate", "ok"].contains(state)
        }
        return kept.union(failed)
    }

    /// What a profile row says: the core left its folder out of the push (it is
    /// Cursor's own folder or repeats an earlier one: the fixed
    /// `Copy.cantAdd`), else its own last round (`rowLine`).
    static func profileRowLine(
        id: String, refused: [String: String], status: CursorSyncStatus.Profile?
    ) -> String? {
        if refused[id] != nil { return Copy.cantAdd.localized }
        return status.flatMap(rowLine)
    }

    /// A profile row's line: the duplicate note, the permanent-failure line,
    /// or the status line for its state.
    static func rowLine(_ status: CursorSyncStatus.Profile) -> String? {
        if status.state == "duplicate" { return Copy.duplicate.localized }
        if let reason = status.reason, permanentReasons.contains(reason) { return Copy.cantRead.localized }
        return statusLine(state: status.state, lastSuccessMs: status.lastSuccessMs, reason: status.reason)
    }

    /// Status line for a core state. nil where there is nothing to say
    /// (`disabled`, not yet synced, or `ok` with no recorded time).
    /// For `error`, `reason` is shown only if it looks like one of the core's
    /// fixed codes (`^[a-z_]{1,40}$`), so the line never carries free text.
    static func statusLine(
        state: String?, lastSuccessMs: Int64?, reason: String? = nil, now: Date = Date()
    ) -> String? {
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
        case "error":
            switch reason {
            case "keychain_consent"?: return Copy.keychainConsent.localized
            case "keychain_denied"?: return Copy.keychainDenied.localized
            case "keychain_unlock"?: return Copy.keychainUnlock.localized
            case "billing_v2_team_required"?: return Copy.billingV2TeamRequired.localized("billing_v2_team_required")
            case "billing_v2_unavailable"?: return Copy.billingV2Unavailable.localized("billing_v2_unavailable")
            case "billing_v2_no_items"?: return Copy.billingV2NoItems.localized("billing_v2_no_items")
            case "billing_v2_principal_mismatch"?:
                return Copy.billingV2PrincipalMismatch.localized("billing_v2_principal_mismatch")
            default: break
            }
            if let reason, (1...40).contains(reason.utf8.count),
               reason.utf8.allSatisfy({ $0 == UInt8(ascii: "_") || (UInt8(ascii: "a")...UInt8(ascii: "z")).contains($0) }) {
                return Copy.errorWithReason.localized(reason)
            }
            return Copy.error.localized
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
    /// The core's fixed reason code for `state` (nil when none).
    @Published private(set) var reason: String?
    @Published private(set) var syncing = false
    /// The last off push could not delete every downloaded file. Kept while
    /// sync is on (the copy says sync is off, so Settings hides it then);
    /// reset when the next off push starts and set from its outcome. Not
    /// persisted: the launch push of an off preference runs the cleanup again.
    @Published private(set) var cleanupFailed = false
    /// The last accepted ON push could not finish deleting profile dirs no
    /// longer listed (a removed profile's, or an earlier appDir's). Set and
    /// cleared by each accepted on push; reset when sync turns off, where
    /// `cleanupFailed` speaks instead.
    @Published private(set) var orphanCleanupFailed = false
    /// Profiles (`default` or a UUID) whose stale usage file a sync round could
    /// not delete and that no later round has cleaned yet (`staleAfter`).
    @Published private(set) var staleProfiles: Set<String> = []
    /// Stale downloaded usage the app knows remains, from either source.
    var staleCleanupFailed: Bool { orphanCleanupFailed || !staleProfiles.isEmpty }
    /// Profiles (by id) the core left out of the last accepted on push, with
    /// the reason; the rest of the push took effect.
    @Published private(set) var refusedProfiles: [String: String] = [:]
    /// The Settings switch was turned on and its confirmation is open. It gates
    /// that switch only: `confirmEnable()` enables nothing unless this is set.
    /// The Overview notice answers through `answerNotice`, a separate path.
    @Published private(set) var confirmPending = false
    /// The last sync's outcome per profile id (`default` or a UUID), so a row
    /// shows its own error even when the headline says something else.
    @Published private(set) var profileStates: [String: CursorSyncStatus.Profile] = [:]
    /// The stored added profiles, published so every view that shows them or
    /// their consent paragraph (the Overview notice card included) re-renders
    /// after an add or a remove.
    @Published private(set) var profiles: [CursorSync.Profile] = CursorSync.profiles()

    private var loop: Task<Void, Never>?
    /// The last config push. Each push waits for the one before it, so rapid
    /// toggles reach the core in the order they were made.
    private var configPush: Task<Void, Never>?
    /// Bumped by every `reconfigure`; a sync result that comes back after a
    /// newer configuration is discarded, so a late result cannot undo an off.
    private var generation = 0
    /// Set when a sync's result was discarded as stale; the sync reruns.
    private var rerunPending = false
    /// An explicit sync was asked for while a round ran (e.g. Allow during a
    /// background round): it runs as soon as that round ends.
    private var explicitPending = false
    /// The "Allow Keychain access…" confirmation is open. `allowKeychainAccess`
    /// grants nothing unless this is set.
    @Published private(set) var keychainConfirmPending = false

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
        sync: @escaping CursorSync.Sync = CursorSync.coreSync,
        setConsent: @escaping @Sendable (String) -> Void = GrokBotKeychainConsent.coreSetConsent
    ) {
        // Test modes make no core call at all: turning off deletes files, and
        // `--demo` runs with the real bundle id. No bundle id (bare `swift
        // run`): no sync there.
        guard CursorSync.isUserRuntime(arguments), let bundleID, !bundleID.isEmpty else { return }
        let json = CursorSync.configJSON(bundleID: bundleID, defaults: defaults, arguments: arguments)
        let listed = CursorSync.profiles(defaults: defaults)
        let run = CursorSync.shouldSync(defaults: defaults, arguments: arguments)
        loop?.cancel()
        generation &+= 1
        let mine = generation
        // An off push retries the cleanup: hide the last failure until it reports.
        if !run {
            state = nil; lastSuccessMs = nil; reason = nil; cleanupFailed = false; orphanCleanupFailed = false
            staleProfiles = []; refusedProfiles = [:]; profileStates = [:]
        }
        let previous = configPush
        let push = Task { [weak self] in
            await previous?.value
            let pushed = await Task.detached(priority: .utility) { setConfig(json) }.value
            // A newer reconfigure (an off push, say) has taken over while this
            // one ran: what it reports describes a configuration that no
            // longer applies, so it changes nothing.
            guard let self, self.generation == mine else { return }
            // nil is a rejected push. A rejected on push is shown (the core
            // kept its previous config, so sync would silently not run),
            // clears the rows' stale outcomes and stops the schedule; the next
            // accepted on push clears it.
            if run {
                if pushed == nil {
                    self.state = CursorSync.configRejectedState
                    self.reason = nil
                    self.profileStates = [:]
                } else if self.state == CursorSync.configRejectedState {
                    self.state = nil
                    self.reason = nil
                }
            }
            // An off push's outcome is `cleanupFailed` (the copy says sync is
            // off); an accepted on push's, which deletes only orphaned profile
            // dirs, is `orphanCleanupFailed`, and its per-entry refusals mark
            // their rows (the other profiles took effect). A rejected push
            // (nil) ran no cleanup and leaves all of these as they were.
            if let pushed {
                if run {
                    self.orphanCleanupFailed = pushed.cleanupFailed
                    self.refusedProfiles = Dictionary(
                        pushed.rejected.compactMap { index, reason in
                            listed.indices.contains(index) ? (listed[index].id, reason) : nil
                        },
                        uniquingKeysWith: { first, _ in first })
                } else {
                    self.cleanupFailed = pushed.cleanupFailed
                }
            }
        }
        configPush = push
        loop = Task { [weak self] in
            await push.value
            if refresh { Self.refreshModel() }
            guard run, self?.state != CursorSync.configRejectedState else { return }
            while !Task.isCancelled {
                await self?.runSync(
                    explicit: false, defaults: defaults, arguments: arguments, sync: sync, setConsent: setConsent)
                try? await Task.sleep(for: .seconds(CursorSync.interval))
            }
        }
    }

    /// Launch (Design 7): install both stored Keychain answers synchronously
    /// (returns whether or not anything was granted), then push the Cursor
    /// config and start the schedule, so a first round that may read the
    /// Keychain already sees the grant.
    func launch(
        defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        bundleID: String? = Bundle.main.bundleIdentifier,
        setConsent: @escaping @Sendable (String) -> Void = GrokBotKeychainConsent.coreSetConsent,
        setConfig: @escaping CursorSync.SetConfig = CursorSync.coreSetConfig,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
    ) {
        GrokBotKeychainConsent.installNow(defaults: defaults, setConsent: setConsent)
        reconfigure(refresh: false, defaults: defaults, arguments: arguments, bundleID: bundleID,
                    setConfig: setConfig, sync: sync, setConsent: setConsent)
    }

    /// "Allow Keychain access…": opens the confirmation with the disclosure.
    func askKeychainAccess() { keychainConfirmPending = true }

    /// Cancel, Escape, clicking away: grants nothing.
    func cancelKeychainAccess() { keychainConfirmPending = false }

    /// Allow in the confirmation (Design 7): store the Cursor answer, install
    /// the composed payload, then run an explicit sync, the one round kind
    /// (besides the first) that may read the Keychain. Does nothing unless the
    /// confirmation asked first.
    func allowKeychainAccess(
        defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        setConsent: @escaping @Sendable (String) -> Void = GrokBotKeychainConsent.coreSetConsent,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
    ) async {
        guard keychainConfirmPending else { return }
        keychainConfirmPending = false
        GrokBotKeychainConsent.setCursorAnswer(true, defaults: defaults)
        GrokBotKeychainConsent.installNow(defaults: defaults, setConsent: setConsent)
        await runSync(explicit: true, defaults: defaults, arguments: arguments, sync: sync, setConsent: setConsent)
    }

    /// One sync, off the main thread. Single-flight here as well as in the core:
    /// a background request while a sync runs is dropped, since that sync's
    /// result is current; an explicit one runs right after it (it may read the
    /// Keychain, which the background round may not). A reconfigure during a
    /// sync makes its result stale; that result is discarded and the sync
    /// reruns, so the new settings get a fresh result. A result reporting
    /// `keychain_denied` withdraws the Cursor Keychain answer
    /// (`revokeCursorIfDenied`, through `setConsent`).
    func runSync(
        explicit: Bool, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync,
        setConsent: @escaping @Sendable (String) -> Void = GrokBotKeychainConsent.coreSetConsent
    ) async {
        guard CursorSync.shouldSync(defaults: defaults, arguments: arguments) else { return }
        if syncing {
            if explicit { explicitPending = true }
            return
        }
        syncing = true
        defer { syncing = false }
        var explicit = explicit
        repeat {
            rerunPending = false
            if explicitPending { explicit = true; explicitPending = false }
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
            let thisExplicit = explicit
            let result = await Task.detached(priority: .utility) { sync(thisExplicit) }.value
            // A reconfigure (e.g. turning sync off) happened while this ran:
            // its result describes a configuration that no longer applies.
            guard started == generation, CursorSync.shouldSync(defaults: defaults, arguments: arguments)
            else { rerunPending = true; continue }
            guard let result else { state = "error"; reason = nil; profileStates = [:]; continue }
            GrokBotKeychainConsent.revokeCursorIfDenied(result, defaults: defaults, setConsent: setConsent)
            state = result.state
            reason = result.reason
            lastSuccessMs = result.lastSuccessMs
            staleProfiles = CursorSync.staleAfter(staleProfiles, status: result)
            profileStates = Dictionary(
                (result.profiles ?? []).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
            // A round that wrote a file already invalidated the core's
            // caches; refresh the views on `changed`, not on the headline (one
            // profile can write while another fails) and not on the event
            // count (another profile can write the same count). The core
            // reports `changed` only when a usage file's bytes changed (or a
            // duplicate profile's old file was removed), so an unchanged
            // history reloads nothing.
            if result.changed { Self.refreshModel() }
        } while (rerunPending || explicitPending) && CursorSync.shouldSync(defaults: defaults, arguments: arguments)
        explicitPending = false
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
    /// uppercase UUID and the path, unless `CursorSync.cantAdd` refuses it
    /// (returns false; nothing is saved). Remove drops the entry. While sync
    /// can't run (off, or the notice unanswered) neither reaches the core (no
    /// config push, no sync, no database read); the stored list is pushed
    /// when sync is turned on. While on, both reconfigure and sync at once;
    /// on a remove the core's orphan cleanup deletes that profile's usage.
    @discardableResult
    func addProfile(
        root: String, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        bundleID: String? = Bundle.main.bundleIdentifier,
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        setConfig: @escaping CursorSync.SetConfig = CursorSync.coreSetConfig,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
    ) -> Bool {
        var list = CursorSync.profiles(defaults: defaults)
        if CursorSync.cantAdd(root: root, existing: list, home: home) { return false }
        list.append(CursorSync.Profile(id: UUID().uuidString, root: (root as NSString).standardizingPath))
        profilesChanged(list, defaults: defaults, arguments: arguments, bundleID: bundleID,
                        setConfig: setConfig, sync: sync)
        return true
    }

    func removeProfile(
        id: String, defaults: UserDefaults = .standard,
        arguments: [String] = CommandLine.arguments,
        bundleID: String? = Bundle.main.bundleIdentifier,
        setConfig: @escaping CursorSync.SetConfig = CursorSync.coreSetConfig,
        sync: @escaping CursorSync.Sync = CursorSync.coreSync
    ) {
        profileStates[id] = nil
        profilesChanged(CursorSync.profiles(defaults: defaults).filter { $0.id != id }, defaults: defaults,
                        arguments: arguments, bundleID: bundleID, setConfig: setConfig, sync: sync)
    }

    /// Save, publish, and reconfigure only through the gate every sync passes.
    private func profilesChanged(
        _ list: [CursorSync.Profile], defaults: UserDefaults, arguments: [String], bundleID: String?,
        setConfig: @escaping CursorSync.SetConfig, sync: @escaping CursorSync.Sync
    ) {
        CursorSync.saveProfiles(list, defaults: defaults)
        profiles = list
        guard CursorSync.shouldSync(defaults: defaults, arguments: arguments) else { return }
        reconfigure(refresh: true, defaults: defaults, arguments: arguments, bundleID: bundleID,
                    setConfig: setConfig, sync: sync)
    }

    /// D6: "Use Syrtis Sync" (true) or "Keep tokscale CLI Data" (false).
    func setTakeoverConfirmed(_ confirmed: Bool, defaults: UserDefaults = .standard) {
        defaults.set(confirmed, forKey: CursorSync.takeoverKey)
        reconfigure(refresh: true, defaults: defaults)
    }
}
