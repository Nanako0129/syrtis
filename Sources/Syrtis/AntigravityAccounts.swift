import Foundation
import TokenBarCore

/// Captured Antigravity accounts: extra Google logins copied from agy, each
/// fetched as its own Antigravity card after the primary.
///
/// UserDefaults holds only `{key, label}` per account. The credential lives in
/// a login-keychain item the core owns (`tb_antigravity_capture`), and the key
/// is a hash of the Google account id, never shown: every label surface goes
/// through `AccountIdentity.accountLabel`, which resolves the key here.
///
/// The core registry is in-memory and starts empty every launch, so the app
/// installs this list at launch and after every change, the same as
/// `ClaudeExtraRoots` and `GrokBotKeychainConsent`.
enum AntigravityAccounts {
    struct Account: Codable, Equatable, Sendable {
        let key: String
        let label: String
    }

    static let storageKey = "tokenbar.antigravity.accounts"

    static func load(defaults: UserDefaults = .standard) -> [Account] {
        decode(defaults.string(forKey: storageKey))
    }

    /// The stored value as a list; `[]` when absent or unreadable.
    static func decode(_ raw: String?) -> [Account] {
        guard let raw,
              let accounts = try? JSONDecoder().decode([Account].self, from: Data(raw.utf8))
        else { return [] }
        return accounts
    }

    static func save(_ accounts: [Account], defaults: UserDefaults = .standard) {
        defaults.set(payloadJSON(accounts), forKey: storageKey)
    }

    /// The stored list is also the `tb_set_antigravity_accounts` payload.
    static func payloadJSON(_ accounts: [Account]) -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = .sortedKeys
        let data = (try? encoder.encode(accounts)) ?? Data("[]".utf8)
        return String(data: data, encoding: .utf8) ?? "[]"
    }

    /// The registry's label for `key`, or nil when the key is not listed.
    static func label(for key: String, defaults: UserDefaults = .standard) -> String? {
        load(defaults: defaults).first { $0.key == key }?.label
    }

    /// Point `AccountIdentity.accountLabel` at this registry. Called at launch;
    /// the selftest passes its own suite.
    static func installLabelResolver(defaults: UserDefaults = .standard) {
        // UserDefaults is documented thread-safe; it is just not marked Sendable.
        nonisolated(unsafe) let defaults = defaults
        AccountIdentity.antigravityLabel = { label(for: $0, defaults: defaults) }
    }

    /// Install the stored list in the core and, when it differs from what this
    /// process installed last, wake the quota pollers so the cards follow.
    static func apply(defaults: UserDefaults = .standard) {
        let payload = payloadJSON(load(defaults: defaults))
        applyQueue.async {
            // The core starts empty each launch, so "nothing installed yet"
            // compares as `[]`: an empty list at launch costs no wake, and a
            // non-empty one wakes the launch poll that may have raced it.
            guard payload != (lastInstalledPayload ?? payloadJSON([])) else {
                lastInstalledPayload = payload
                return
            }
            // Cache only what the core accepted, so a failed install is
            // retried on the next `apply()` instead of being skipped as done.
            guard (try? TBCore.setAntigravityAccounts(json: payload)) != nil else { return }
            lastInstalledPayload = payload
            Task { @MainActor in
                // Invalidate before signalling, as `ClaudeExtraRoots.install`
                // does: a woken poll must not be answered from the throttled
                // payload built for the previous account set.
                await AgentUsageThrottle.shared.invalidate()
                ClaudeExtraRoots.RegistryChange.signal()
            }
        }
    }

    private static let applyQueue = DispatchQueue(
        label: "com.nyanako.tokenbar.antigravity-accounts", qos: .userInitiated)
    /// Touched only on `applyQueue`.
    nonisolated(unsafe) private static var lastInstalledPayload: String?

    /// `accounts` with `captured` added, or its label refreshed when the same
    /// Google account was captured before.
    static func adding(_ captured: Account, to accounts: [Account]) -> [Account] {
        guard let index = accounts.firstIndex(where: { $0.key == captured.key }) else {
            return accounts + [captured]
        }
        var updated = accounts
        updated[index] = captured
        return updated
    }

    /// A short sentence for a capture or remove failure. The core's error is a
    /// fixed code naming no account; it is mapped here and never shown raw.
    static func message(for error: Error) -> String {
        guard case let TBCoreError.bridge(code) = error else {
            return "Something went wrong. Try again."
        }
        switch code {
        case "agy_not_signed_in":
            return "Couldn't read agy's login. Check that agy is signed in, and allow access if macOS asks."
        case "agy_login_unreadable":
            return "agy's saved login is in a format Syrtis doesn't recognize."
        case "agy_login_missing_identity":
            return "agy's saved login doesn't say which Google account it is. Sign agy in again, then try again."
        case "oauth_client_not_found":
            return "Couldn't match this login to the installed Antigravity or agy."
        case "oauth_client_rejected", "refresh_rejected":
            return "Google didn't accept this login. Sign agy in again, then try again."
        case "refresh_unreachable":
            return "Google couldn't be reached right now. Try again later."
        case "account_mismatch":
            return "Google answered for a different account. Nothing was saved."
        case "invalid_credential_format":
            return "The login had an unexpected format. Nothing was saved."
        case "keychain_write_failed":
            return "Couldn't save to the login keychain."
        case "keychain_delete_failed":
            return "Couldn't delete the copy from the login keychain."
        default:
            return "Something went wrong. Try again."
        }
    }
}

// MARK: - Registry mutations and removed keys (S4)

extension AntigravityAccounts {
    /// Keys (hashes only) the user removed while automatic capture was on.
    /// Automatic capture skips them before any request; only a manual
    /// Capture of that account takes a key off this list.
    static let removedKeysKey = "tokenbar.antigravity.removedKeys"

    static func removedKeys(defaults: UserDefaults = .standard) -> [String] {
        defaults.stringArray(forKey: removedKeysKey) ?? []
    }

    /// The one path every registry change goes through. MainActor-isolated and
    /// synchronous: it re-reads UserDefaults, applies `change`, saves and
    /// installs with no suspension in between, so two changes cannot
    /// interleave and none starts from a stale copy (a Settings view's, or a
    /// capture that finished after a remove).
    @MainActor
    static func mutate(
        defaults: UserDefaults = .standard,
        install: (UserDefaults) -> Void = { apply(defaults: $0) },
        _ change: ([Account]) -> [Account]
    ) {
        save(change(load(defaults: defaults)), defaults: defaults)
        install(defaults)
    }
}

// MARK: - Automatic capture (S4)

/// Captures every Google account agy signs into, without a button press,
/// while the Settings toggle `enabledKey` is on (off by default).
///
/// Trigger: before each quota fetch, both poll loops await
/// `TrayAnimator.prepareAntigravityAutoCapture`, which runs `prepareForFetch()`
/// only when the toggle is on. `poll()` reads agy's login
/// marker (attributes only, no secret) and, when it differs from the last
/// marker attempted, runs ONE automatic capture in the core. The marker is
/// recorded before the attempt, so a failure is not retried until the marker
/// changes again, the toggle is turned on again, or the user presses Capture.
///
/// `currentAgyKey` is the key of the account agy is signed into, bound to the
/// login marker it was confirmed under. It drives `AntigravityDedup`, which
/// also requires the primary card's marker to match, so a binding never
/// labels a card fetched under another login. While automatic capture is on
/// it is cleared the moment a new marker is seen (before the attempt) and on
/// a pause, and set by a `captured` / `unchanged` attempt. A successful
/// manual Capture sets it whether or not the toggle is on, when agy's marker
/// did not change during the capture. Turning the toggle off keeps it; Remove
/// of that account clears it. It is persisted (`currentKey`) across relaunch.
///
/// MainActor-isolated rather than a plain `actor`: the publication
/// coordinator reads `currentAgyKey` synchronously on the MainActor. The
/// blocking core calls run on detached tasks. One operation at a time:
/// `busy` covers the automatic attempt, manual Capture and Remove, so a
/// capture can never land between a remove's keychain delete and its
/// registry change.
@MainActor
final class AntigravityAutoCapture: ObservableObject {
    struct IO {
        var marker: @Sendable () throws -> String
        var autoCapture: @Sendable ([String]) throws -> AntigravityAutoCaptureResult
        var capture: @Sendable () throws -> AntigravityCapturedAccount
        var remove: @Sendable (String) throws -> Void
        /// Install the stored list in the core (`AntigravityAccounts.apply`).
        var install: @MainActor (UserDefaults) -> Void

        static let live = IO(
            marker: { try TBCore.antigravityLoginMarker() },
            autoCapture: { try TBCore.antigravityAutoCapture(removedKeys: $0) },
            capture: { try TBCore.antigravityCapture() },
            remove: { try TBCore.antigravityRemove(key: $0) },
            install: { AntigravityAccounts.apply(defaults: $0) })
    }

    static let enabledKey = "tokenbar.antigravity.autoCapture"

    /// Replaced only by the selftest, with a fake `IO` and a throwaway suite.
    static var shared = AntigravityAutoCapture(io: .live, defaults: .standard)

    let io: IO
    let defaults: UserDefaults

    private(set) var lastAttemptedMarker: String?
    private(set) var currentAgyKey: String? {
        didSet {
            persistCurrent()
            // Wake the pollers so the dedup follows now, not a cycle later.
            if currentAgyKey != oldValue { ClaudeExtraRoots.RegistryChange.signal() }
        }
    }

    /// `currentAgyKey` with the marker it was confirmed under, kept across
    /// relaunches: a hash and a keychain modification date, no secret. Safe to
    /// restore without re-reading agy's login, because dedup also requires the
    /// primary card to have been fetched under that same marker, and any agy
    /// sign-in change since moves the marker.
    static let currentKey = "tokenbar.antigravity.currentAgy"

    private func persistCurrent() {
        if let key = currentAgyKey {
            defaults.set(["key": key, "marker": currentAgyMarker ?? ""], forKey: Self.currentKey)
        } else {
            defaults.removeObject(forKey: Self.currentKey)
        }
    }
    @Published private(set) var busy = false
    private var checking = false
    private var pollAgain = false
    @Published private(set) var paused = false
    /// The marker query cannot see a modification date on this Mac, so a
    /// login change cannot be detected.
    @Published private(set) var unavailable = false
    /// A sentence from `AntigravityAccounts.message(for:)` for the last manual
    /// Capture or Remove that failed; never a raw code.
    @Published private(set) var message: String?

    init(io: IO, defaults: UserDefaults) {
        self.io = io
        self.defaults = defaults
        if let stored = defaults.dictionary(forKey: Self.currentKey) as? [String: String],
           let key = stored["key"], let marker = stored["marker"], !marker.isEmpty {
            currentAgyMarker = marker
            currentAgyKey = key
        }
    }

    var isEnabled: Bool { defaults.bool(forKey: Self.enabledKey) }

    /// One check, from a quota poll or right after the toggle turns on. The
    /// caller gates on the toggle; this only refuses to overlap and to run
    /// while paused.
    func poll(marker known: String? = nil) async {
        guard !paused else { return }
        // A poll refused because something is running (the toggle turned on
        // mid-attempt, say) is owed, and runs when that work ends.
        guard !busy, !checking else {
            pollAgain = true
            return
        }
        // The marker check alone is not `busy`: Settings shows "Capturing…"
        // only for an actual capture, not every five minutes.
        let io = io
        var marker = known
        if marker == nil {
            checking = true
            marker = try? await Self.detached({ try io.marker() })
            checking = false
        }
        guard let marker else { return await pollIfOwed() }
        unavailable = marker == "present"
        guard marker != lastAttemptedMarker, !busy else { return await pollIfOwed() }
        busy = true
        // Both before the attempt: the old key may not be agy's account any
        // more, and a failed attempt must not be retried for this marker.
        currentAgyKey = nil
        lastAttemptedMarker = marker
        let removed = AntigravityAccounts.removedKeys(defaults: defaults)
        let result = await Self.detached { Result { try io.autoCapture(removed) } }
        switch result {
        case let .success(outcome):
            guard let key = outcome.key, let label = outcome.label,
                  outcome.status == "captured" || outcome.status == "unchanged"
            else { break }
            AntigravityAccounts.mutate(defaults: defaults, install: io.install) { accounts in
                // `unchanged` keeps a label already listed; only a fresh
                // capture refreshes it.
                outcome.status == "unchanged" && accounts.contains { $0.key == key }
                    ? accounts
                    : AntigravityAccounts.adding(.init(key: key, label: label), to: accounts)
            }
            if isEnabled {
                // Bound only if agy's marker did not move during the attempt,
                // as `manualCapture`: the key is the account agy was signed
                // into when the core read it, and a sign-in that landed
                // meanwhile would label the next login's card with this one's
                // email. Moved: left unbound, and the next check sees the new
                // marker and attempts again. Unreadable: left unbound and the
                // attempted marker forgotten, so the next check retries this
                // marker instead of waiting for agy's login to change (the
                // core answers `unchanged` with no Google request while its
                // stored token equals agy's). Syrtis-Windows W7b does the same.
                let after = try? await Self.detached({ try io.marker() })
                if after == marker {
                    setCurrent(key, marker: marker)
                } else if after == nil {
                    lastAttemptedMarker = nil
                }
            }
        case .failure(TBCoreError.bridge("paused")):
            paused = true
            currentAgyKey = nil
        case .failure:
            break
        }
        busy = false
        await pollIfOwed()
    }

    /// The marker agy's login item carried when `currentAgyKey` was
    /// confirmed. Dedup also requires the primary card to have been fetched
    /// under this same marker: a card fetched before an agy sign-in change is
    /// never labelled as the account signed in after it (observed on the test
    /// bundle: B's quota shown under A's email right after agy switched to A).
    private(set) var currentAgyMarker: String?

    private func setCurrent(_ key: String, marker: String?) {
        currentAgyMarker = marker
        currentAgyKey = key
    }

    /// Before a quota fetch, awaited by both poll loops: read agy's login
    /// marker (attributes only, milliseconds) and, when it differs from the
    /// last attempt, forget the current account NOW. Without this the fetch
    /// that follows a login change in agy is drawn as the previous account
    /// until the next check, up to one tray cycle later: observed on the test
    /// bundle, where the primary card showed account A's quota under B's email
    /// and B's own card was hidden as its duplicate. The capture attempt is
    /// returned, not awaited, so the fetch never waits on Google.
    func prepareForFetch() async -> Task<Void, Never>? {
        guard !paused, !checking else { return nil }
        checking = true
        let io = io
        let marker = try? await Self.detached({ try io.marker() })
        checking = false
        guard let marker else { return nil }
        if marker != lastAttemptedMarker { currentAgyKey = nil }
        return Task { await self.poll(marker: marker) }
    }

    private func pollIfOwed() async {
        guard pollAgain else { return }
        pollAgain = false
        await poll()
    }

    /// The Settings toggle. On: forget the last marker and try now. Off: stop
    /// watching agy's login; the current account stays, still bound to its
    /// marker, so a manual capture's dedup survives (maintainer decision
    /// 2026-10-03). Either way a pause ends.
    func setEnabled(_ on: Bool) async {
        defaults.set(on, forKey: Self.enabledKey)
        paused = false
        if on {
            lastAttemptedMarker = nil
            await poll()
        } else {
            unavailable = false
        }
    }

    /// Capture agy's current login (the Settings button). Takes the account
    /// off the removed list, ends a pause, and then tries automatic capture
    /// once more when it is on.
    func manualCapture() async {
        guard !busy else { return }
        busy = true
        message = nil
        let io = io
        // The marker before AND after the capture: the Settings steps say to
        // sign agy back in right after pressing Capture, and a sign-in that
        // lands while the capture runs would otherwise bind the captured key
        // to the NEXT login's marker, labelling that account's card with this
        // one's email (verifier advisory, 2026-10-03). Bound only if equal.
        let markerBefore = try? await Self.detached({ try io.marker() })
        let result = await Self.detached { Result { try io.capture() } }
        var resume = false
        switch result {
        case let .success(captured):
            AntigravityAccounts.mutate(defaults: defaults, install: io.install) {
                AntigravityAccounts.adding(.init(key: captured.key, label: captured.label), to: $0)
            }
            defaults.set(
                AntigravityAccounts.removedKeys(defaults: defaults).filter { $0 != captured.key },
                forKey: AntigravityAccounts.removedKeysKey)
            // This read agy's login as it is now, so the key is agy's account,
            // under the marker agy's item carries now. Whether or not automatic
            // capture is on (maintainer decision 2026-10-03): the button is the
            // consent, and no further secret is read for it. When agy signs in
            // elsewhere the marker moves and dedup stops by itself.
            let markerAfter = try? await Self.detached({ try io.marker() })
            if let markerBefore, markerBefore == markerAfter {
                setCurrent(captured.key, marker: markerAfter)
            } else {
                currentAgyKey = nil
            }
            if paused {
                paused = false
                lastAttemptedMarker = nil
                resume = isEnabled
            }
        case let .failure(error):
            message = AntigravityAccounts.message(for: error)
        }
        busy = false
        if resume { await poll() }
        await pollIfOwed()
    }

    /// Delete one account's keychain copy and drop it from the list. While
    /// automatic capture is on, the key also goes on the removed list so the
    /// next login change does not add it back.
    func remove(_ account: AntigravityAccounts.Account) async {
        guard !busy else { return }
        busy = true
        message = nil
        let io = io
        let key = account.key
        let result = await Self.detached { Result { try io.remove(key) } }
        switch result {
        case .success, .failure(TBCoreError.bridge("invalid_key")):
            // An invalid key has no keychain item and no card: the core
            // registry rejects it. Dropping the row is all that is left.
            AntigravityAccounts.mutate(defaults: defaults, install: io.install) {
                $0.filter { $0.key != key }
            }
            if currentAgyKey == key { currentAgyKey = nil }
            if isEnabled {
                let removed = AntigravityAccounts.removedKeys(defaults: defaults)
                if !removed.contains(key) {
                    defaults.set(removed + [key], forKey: AntigravityAccounts.removedKeysKey)
                }
            }
        case let .failure(error):
            message = AntigravityAccounts.message(for: error)
        }
        busy = false
        await pollIfOwed()
    }

    private static func detached<T: Sendable>(
        _ work: @escaping @Sendable () throws -> T
    ) async throws -> T {
        try await Task.detached(priority: .userInitiated) { try work() }.value
    }

    private static func detached<T: Sendable>(_ work: @escaping @Sendable () -> T) async -> T {
        await Task.detached(priority: .userInitiated) { work() }.value
    }

    /// Test seam only: a fresh state with nothing attempted.
    func resetForTesting() {
        lastAttemptedMarker = nil
        currentAgyKey = nil
        busy = false
        checking = false
        pollAgain = false
        paused = false
        unavailable = false
        message = nil
    }
}

// MARK: - One card for agy's current account (S4)

/// Once agy's current account is known (automatic capture, or a manual
/// Capture), the account agy is signed into is also a captured account, so it would be drawn twice: once as the primary card
/// (the agy route) and once as its captured card. This drops the captured
/// card and labels the primary with its email, ONLY when all of these hold:
/// - `currentAgyKey` is set (verified for the current agy login marker);
/// - the primary Antigravity snapshot (`accountKey == nil`) came from the agy
///   route (`source == "agy"`) and has no error, so it is agy's account;
/// - a captured snapshot carries that key.
/// Otherwise (IDE `cli` or `oauth` source, any error) both are shown.
///
/// The primary keeps its own windows and values (gauge, tray, selection), but
/// the agy route has no trusted history identity, so its windows carry no
/// history key. When the captured account's snapshot has no error, the merged
/// primary adopts that account's pace status, historical pace and the window
/// duration they describe (the engine clears the agy primary's duration with
/// the `accountScope` mark) per matching card id, and records it as
/// `historyAccountKey`, so the card's curve, cycles
/// and strip are read under the captured account's own scope
/// (`AgentUsageSnapshot.adoptingHistory(of:)`). `accountKey` stays nil and no
/// scope is rewritten; the captured account is still fetched and recorded in
/// the core. If it errored or has no windows the primary is left as it was.
/// Applied to BOTH `AgentUsagePublicationCoordinator.resolve` and
/// `.latestPayload`, which every quota consumer reads. Idempotent.
enum AntigravityDedup {
    static func apply(
        _ payload: AgentUsagePayload, currentAgyKey: String?, currentAgyMarker: String?
    ) -> AgentUsagePayload {
        guard let key = currentAgyKey,
              let marker = currentAgyMarker, marker != "present",
              let primaryIndex = payload.agents.firstIndex(where: {
                  $0.clientId == "antigravity" && $0.accountKey == nil
              }),
              payload.agents[primaryIndex].source == "agy",
              payload.agents[primaryIndex].agyLoginMarker == marker,
              payload.agents[primaryIndex].error == nil,
              let capturedIndex = payload.agents.firstIndex(where: {
                  $0.clientId == "antigravity" && $0.accountKey == key
              })
        else { return payload }
        var agents = payload.agents
        let primary = agents[primaryIndex]
        let captured = agents[capturedIndex]
        var merged = primary
        if let label = captured.identity?.email {
            merged = merged.replacingIdentity(.make(email: label, plan: primary.identity?.plan))
        }
        agents[primaryIndex] = merged.adoptingHistory(of: captured)
        agents.remove(at: capturedIndex)
        return payload.replacingAgents(agents)
    }
}
