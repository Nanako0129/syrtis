import CTB
import Foundation
import os

/// FFI-boundary log. Backend failures and contract drift would otherwise vanish
/// — every app-side caller wraps these in `try?` to keep the last good numbers.
/// Agent usage is the sensitive exception: it logs only bounded transport fields
/// plus fixed outer-failure classifications, never backend or decoding text.
let ffiLog = Logger(subsystem: "com.nyanako.tokenbar", category: "ffi")

/// Errors crossing the Rust FFI boundary.
public enum TBCoreError: Error {
    case nullPointer
    case bridge(String)
}

package enum AgentUsageBoundaryLogEvent: Equatable, Sendable {
    case returnedNull
    case bridgeFailed
    case decodeFailed
}

/// Result of the `tb_probe` smoke entry point.
public struct ProbeResult: Decodable {
    public let ok: Bool
    public let messages: Int?
    public let err: String?
}

/// Standard envelope every non-probe entry point returns:
/// `{"ok":true,"data":<payload>}` or `{"ok":false,"err":"..."}`.
struct TBEnvelope<T: Decodable>: Decodable {
    let ok: Bool
    let data: T?
    let err: String?
    /// Whether `data` was present at all, which `data: T?` alone cannot report:
    /// synthesized Optional decoding maps both an absent key and an explicit
    /// null to nil. Only `decodeOptionalEnvelope` needs the distinction — every
    /// other entry point rejects a nil payload outright — but the envelope is
    /// where the fact lives.
    let hasDataKey: Bool

    private enum CodingKeys: String, CodingKey {
        case ok, data, err
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.ok = try container.decode(Bool.self, forKey: .ok)
        self.data = try container.decodeIfPresent(T.self, forKey: .data)
        self.err = try container.decodeIfPresent(String.self, forKey: .err)
        self.hasDataKey = container.contains(.data)
    }
}

/// A single extra scan path with a note attached: `client`/`path` identify
/// it, `reason` explains why it's in this list.
public struct ScanPathNote: Decodable, Equatable, Sendable {
    public let client: String
    public let path: String
    public let reason: String
}

/// Result of `tb_set_extra_scan_paths`.
public struct ExtraScanPathsResult: Decodable, Equatable, Sendable {
    /// Paths actually entered into the scan registry, including ones
    /// currently listed in `unreadable`.
    public let registeredCount: Int
    /// Registered, but `read_dir` failed for it right now (unmounted volume,
    /// not-yet-created config dir). Retried automatically on the next scan —
    /// no action needed from the user beyond fixing the underlying cause.
    public let unreadable: [ScanPathNote]
    /// NOT registered, and never will be without the user acting: either the
    /// client id has no extra-root support here, or the path cannot be a scan
    /// root at all (empty, relative, or something that exists but isn't a
    /// directory). Unlike `unreadable`, waiting does not fix these.
    public let rejected: [ScanPathNote]
}

/// One directory `tb_set_claude_config_dirs` refused, and why. Unlike an
/// unreadable scan root, nothing here fixes itself by waiting: the path is
/// empty, relative, the filesystem root, or a repeat of one already listed.
public struct RejectedConfigDir: Decodable, Equatable, Sendable {
    public let path: String
    public let reason: String
}

/// Result of `tb_set_claude_config_dirs`.
public struct ClaudeConfigDirsResult: Decodable, Equatable, Sendable {
    /// Directories now fetched as their own Claude quota card.
    public let registeredCount: Int
    public let rejected: [RejectedConfigDir]
}

/// One entry `tb_set_antigravity_accounts` refused, by its position in the
/// input and a fixed reason. The entry itself is never echoed back.
public struct RejectedAntigravityAccount: Decodable, Equatable, Sendable {
    public let index: Int
    public let reason: String
}

/// Result of `tb_set_antigravity_accounts`.
public struct AntigravityAccountsResult: Decodable, Equatable, Sendable {
    /// Captured accounts now fetched as their own Antigravity card.
    public let registeredCount: Int
    public let rejected: [RejectedAntigravityAccount]
}

/// Success data of `tb_antigravity_capture`: the new account's registry key
/// (64 hex, not for display) and its label (the Google email, or a fallback).
public struct AntigravityCapturedAccount: Decodable, Equatable, Sendable {
    public let key: String
    public let label: String

    public init(key: String, label: String) {
        self.key = key
        self.label = label
    }
}

/// Success data of `tb_set_antigravity_binding`.
package struct AntigravityBindingResult: Decodable {
    package let bound: Bool
}

/// Success data of `tb_antigravity_remove`.
package struct AntigravityRemoved: Decodable {
    package let removed: Bool
}

/// Success data of `tb_antigravity_login_marker`: the login item's
/// modification date, `present`, or `absent`. Not a secret.
package struct AntigravityLoginMarker: Decodable {
    package let marker: String
}

/// Success data of `tb_antigravity_auto_capture`. `key` and `label` are
/// present for `captured` and `unchanged`, absent for `skipped_removed`.
public struct AntigravityAutoCaptureResult: Decodable, Equatable, Sendable {
    public let status: String
    public let key: String?
    public let label: String?

    public init(status: String, key: String? = nil, label: String? = nil) {
        self.status = status
        self.key = key
        self.label = label
    }
}

/// One client id `tb_set_keychain_consent` refused, and why: the core wires
/// Keychain consent for a fixed set of clients, and a grant nothing reads
/// would claim the user was asked about a dialog that still appears unasked.
public struct RejectedKeychainConsent: Decodable, Equatable, Sendable {
    public let client: String
    public let reason: String
}

/// Result of `tb_set_keychain_consent`.
public struct KeychainConsentResult: Decodable, Equatable, Sendable {
    /// Clients this process may now read a Keychain item for. A WIRED client
    /// not counted here is not reached, so macOS is not given the chance to
    /// raise its authorization dialog for that client. Says nothing about
    /// clients this registry does not wire — the Claude provider reads its own
    /// Keychain items ungated.
    public let grantedCount: Int
    public let rejected: [RejectedKeychainConsent]
}

/// Result of `tb_set_cursor_sync`.
public struct CursorSyncConfigResult: Decodable, Equatable, Sendable {
    public let enabled: Bool
    public let dir: String?
    public let cliTakeoverConfirmed: Bool
    /// Syrtis usage files deleted because sync was turned off or moved.
    public let removedFiles: Int
    /// Some of those files could not be deleted.
    public let cleanupFailed: Bool
}

/// Result of `tb_cursor_sync`.
public struct CursorSyncStatus: Decodable, Equatable, Sendable {
    /// `ok | partial | expired | notSignedIn | offline | error | disabled | cliPresent`.
    public let state: String
    /// Events written by this call; 0 unless it completed.
    public let events: Int
    /// The complete synced file's mtime, if one exists.
    public let lastSuccessMs: Int64?
    /// A fixed diagnostic code (e.g. `rate_limited`); never account data.
    public let reason: String?
}

/// Thin Swift facade over the tb_core_ffi staticlib. All calls are blocking;
/// invoke from a background thread/actor in app code. `agentUsage()` is also
/// network-bound.
public enum TBCore {
    /// Copy the FFI string out of the heap buffer and free it, so decoding never
    /// races the C allocation. Returns nil for a NULL pointer. This is the single
    /// legal consumer of a tb_* return pointer — `tb_free` happens here exactly
    /// once, on every path (the `defer`), which is what keeps the boundary free
    /// of leaks and double-frees.
    private static func takeBytes(_ raw: UnsafeMutablePointer<CChar>?) -> Data? {
        guard let raw else { return nil }
        defer { tb_free(raw) }
        return Data(bytes: raw, count: strlen(raw))
    }

    /// Decode a bare JSON payload returned by a tb_* entry point, then free it.
    /// (Used by the legacy `tb_probe` shape; enveloped entry points use `unwrap`.)
    static func decode<T: Decodable>(_ raw: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let data = takeBytes(raw) else {
            ffiLog.error("FFI returned NULL for \(String(describing: T.self), privacy: .public)")
            throw TBCoreError.nullPointer
        }
        do {
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            ffiLog.error(
                "FFI decode \(String(describing: T.self), privacy: .public) failed: \(String(describing: error), privacy: .public)")
            throw error
        }
    }

    /// Decode an enveloped payload, surfacing `{"ok":false}` as a thrown error.
    /// Like `unwrap`, but a successful envelope may legitimately carry `null`.
    /// `tb_quota_curve` uses that for "this series is bound but has no history
    /// yet", which is an answer rather than a failure. Kept as its own path so
    /// every other entry point still treats a missing payload on `ok:true` as a
    /// decode failure.
    static func unwrapOptional<T: Decodable>(_ raw: UnsafeMutablePointer<CChar>?) throws -> T? {
        guard let data = takeBytes(raw) else {
            ffiLog.error("FFI returned NULL for \(String(describing: T.self), privacy: .public)")
            throw TBCoreError.nullPointer
        }
        return try decodeOptionalEnvelope(data)
    }

    package static func decodeOptionalEnvelope<T: Decodable>(_ data: Data) throws -> T? {
        let envelope = try JSONDecoder().decode(TBEnvelope<T>.self, from: data)
        guard envelope.ok else {
            throw TBCoreError.bridge(envelope.err ?? "unknown")
        }
        // `data: T?` cannot tell an explicit null from an absent key — synthesized
        // Optional decoding accepts both — so the key's presence is checked
        // separately. Rust always emits it (`envelope()` builds
        // `{"ok":true,"data":<value>}` and `Value::Null` serializes as `"data":null`),
        // which makes a missing key ABI drift rather than "no history". Without
        // this, that drift would read as an empty curve and disappear.
        guard envelope.hasDataKey else {
            throw DecodingError.dataCorrupted(.init(
                codingPath: [],
                debugDescription: "successful FFI envelope is missing the data key"))
        }
        return envelope.data
    }

    static func unwrap<T: Decodable>(_ raw: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let data = takeBytes(raw) else {
            ffiLog.error("FFI returned NULL for \(String(describing: T.self), privacy: .public)")
            throw TBCoreError.nullPointer
        }
        do {
            return try decodeEnvelope(data)
        } catch {
            ffiLog.error(
                "FFI \(String(describing: T.self), privacy: .public) failed: \(String(describing: error), privacy: .public)")
            throw error
        }
    }

    /// Pure envelope decode: `{"ok":true,"data":..}` → payload, `{"ok":false}` →
    /// `TBCoreError.bridge`, and a successful envelope without data → decoding
    /// contract failure. Split out from the pointer/free path so the
    /// error contract is unit-testable (`envelopeContractChecks`) without a real
    /// FFI allocation — feeding a synthetic pointer to `decode` would be unsound,
    /// since `tb_free` must only ever release a Rust-allocated pointer.
    package static func decodeEnvelope<T: Decodable>(_ data: Data) throws -> T {
        let envelope = try JSONDecoder().decode(TBEnvelope<T>.self, from: data)
        guard envelope.ok else {
            throw TBCoreError.bridge(envelope.err ?? "unknown")
        }
        guard let payload = envelope.data else {
            throw DecodingError.dataCorrupted(.init(
                codingPath: [],
                debugDescription: "successful FFI envelope is missing data"))
        }
        return payload
    }

    /// Pass an optional year filter across the boundary (nil = all time).
    private static func withYear<R>(
        _ year: String?, _ body: (UnsafePointer<CChar>?) -> R
    ) -> R {
        guard let year else { return body(nil) }
        return year.withCString { body($0) }
    }

    /// Pass an optional year filter and optional client filter across the
    /// boundary. nil year = all time; nil OR empty clients = all clients (the
    /// FFI treats a NULL/empty client arg as "every client"). Client ids are
    /// comma-joined. NOTE: an empty selection therefore reaches the core as
    /// "all clients", not "no clients" — the all-hidden case is enforced by the
    /// lens views' strict membership filter, not here.
    private static func withYearAndClients<R>(
        _ year: String?, _ clients: [String]?,
        _ body: (UnsafePointer<CChar>?, UnsafePointer<CChar>?) -> R
    ) -> R {
        let joined = (clients?.isEmpty ?? true) ? nil : clients!.joined(separator: ",")
        return withYear(year) { yearPtr in
            guard let joined else { return body(yearPtr, nil) }
            return joined.withCString { body(yearPtr, $0) }
        }
    }

    public static func probe() throws -> ProbeResult {
        let result: ProbeResult = try decode(tb_probe())
        if !result.ok { throw TBCoreError.bridge(result.err ?? "unknown") }
        return result
    }

    /// Contribution graph for `year` (nil = all time). Served from a <=30s
    /// cache inside the staticlib when warm.
    public static func graph(year: String? = nil) throws -> UsagePayload {
        try unwrap(withYear(year) { tb_graph($0) })
    }

    /// Contribution graph, always recomputed.
    public static func refreshGraph(year: String? = nil) throws -> UsagePayload {
        try unwrap(withYear(year) { tb_refresh_graph($0) })
    }

    public static func modelReport(year: String? = nil) throws -> ModelReport {
        try unwrap(withYear(year) { tb_model_report($0) })
    }

    /// Per-hour report for `year` (nil = all time), restricted to `clients`
    /// (nil/empty = all clients). The core filters at the streaming scan, so a
    /// client slice yields accurate per-client totals for hours shared across
    /// clients (a downstream membership filter cannot — buckets fold all
    /// clients into one mixed total).
    public static func hourlyReport(year: String? = nil, clients: [String]? = nil) throws -> HourlyReport {
        try unwrap(withYearAndClients(year, clients) { tb_hourly_report($0, $1) })
    }

    /// Per-agent report for `year` (nil = all time), restricted to `clients`
    /// (nil/empty = all clients). Scan-level filter, same rationale as
    /// `hourlyReport`.
    public static func agentsReport(year: String? = nil, clients: [String]? = nil) throws -> AgentsReport {
        try unwrap(withYearAndClients(year, clients) { tb_agents_report($0, $1) })
    }

    /// Source-generation-aware parity diagnostic for the hourly and Agents
    /// filter. Rust derives the client list from a fresh graph and brackets all
    /// report calls with one opaque local-source token sequence.
    public static func filterParityProbe() throws -> FilterParityProbe {
        try unwrap(tb_filter_parity_probe())
    }

    /// Read-only quota curve snapshot for one bound series. `generation` must be
    /// the publication generation the series identity was bound under; a stale
    /// one, or a series this process never verified, fails closed rather than
    /// serving a curve.
    ///
    /// Returns nil when the series exists but has no stored history yet.
    /// PROTOTYPE — usage inside an absolute [from, until) window, for one
    /// account.
    ///
    /// `accountKey` is the same value `quotaCurve` takes: `nil` for the primary
    /// account, an extra Claude account's config directory otherwise. It scopes
    /// which transcripts are read, so the usage folded into a window and the
    /// quota that window reports come from one account.
    ///
    /// There is no argument for "every account", and no default for this one.
    /// A window belongs to an account; a total spanning accounts has no quota
    /// reading to divide by, and a caller that omitted the argument would get
    /// every account's usage measured against one account's allowance — which
    /// is the defect in issue #258, and the reason the compiler is made to ask.
    public static func windowUsage(
        accountKey: String?, from: Int64, until: Int64
    ) throws -> WindowUsage {
        // `withCString` on an Optional would bind a temporary that dies before
        // the call; the nil case has to pass a real NULL.
        if let accountKey {
            return try accountKey.withCString { account in
                try unwrap(tb_window_usage(account, from, until))
            }
        }
        return try unwrap(tb_window_usage(nil, from, until))
    }

    /// `accountKey` selects which account's series to read. `nil` is the
    /// primary account, which is every account that exists today; an extra
    /// Claude account passes the config directory its binding was published
    /// under.
    ///
    /// It is a parameter rather than something the core infers, because the
    /// failure it prevents is silent: with one account per client the binding
    /// map used to be keyed on `(client, window)`, and a second account
    /// publishing the same window would replace the first. The call would then
    /// answer with the other account's curve under a generation that
    /// validates — nothing on either side of the FFI could tell.
    // No default: a caller that omits this reads the primary account's curve
    // and nothing reports the mistake, so the argument is required.
    public static func quotaCurve(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) throws -> QuotaCurve? {
        let curve: QuotaCurve? = try clientId.withCString { client in
            try windowKey.withCString { window in
                // `withCString` on an Optional would bind a temporary that dies
                // before the call; the nil case has to pass a real NULL.
                if let accountKey {
                    return try accountKey.withCString { account in
                        try unwrapOptional(tb_quota_curve(client, account, window, generation))
                    }
                }
                return try unwrapOptional(tb_quota_curve(client, nil, window, generation))
            }
        }
        try requireAnswering(curve, request: generation)
        return curve
    }

    /// Rust stamps the payload with the generation the call passed in, so this
    /// is the one field the caller can check against something it already knows.
    /// A mismatch means the response did not answer this request, which no
    /// amount of structural validation inside the payload could reveal.
    package static func requireAnswering(_ curve: QuotaCurve?, request: UInt64) throws {
        guard let curve, curve.generation != request else { return }
        throw TBCoreError.bridge(
            "quota curve generation \(curve.generation) does not answer request \(request)")
    }

    /// Live trace buckets over the trailing `windowSecs`.
    public static func usageTrace(windowSecs: Int64) throws -> [TraceBucket] {
        try unwrap(tb_usage_trace(windowSecs))
    }

    /// The quota provider ids, in card order, from the engine's single
    /// registration table. Offline and cheap.
    public static func quotaProviderIds() throws -> [String] {
        let payload: QuotaProviderIds = try unwrap(tb_quota_provider_ids())
        return payload.ids
    }

    /// Every local client id the engine attributes usage to. Offline and cheap.
    public static func engineClientIds() throws -> [String] {
        let payload: QuotaProviderIds = try unwrap(tb_client_ids())
        return payload.ids
    }

    /// Live tokens/min estimate (10-minute-window average).
    public static func tokensPerMin() throws -> Double {
        let payload: TokensPerMin = try unwrap(tb_tokens_per_min())
        return payload.tokensPerMin
    }

    /// Replace the process-wide extra-scan-paths registry. `json` is an object
    /// of `{"<public-client-id>": ["<absolute-dir-path>", ...]}`; full-replace
    /// semantics — passing `{}` clears every configured root. Takes effect on
    /// the very next report/parse call, no restart needed. A directory that
    /// merely can't be read right now is still registered — `unreadable` lists
    /// those, and the next scan retries them automatically with no further
    /// action. `rejected` lists paths that are NOT registered: an unsupported
    /// client id, or a path that can never be a scan root (empty, relative, or
    /// an existing non-directory).
    public static func setExtraScanPaths(json: String) throws -> ExtraScanPathsResult {
        try unwrap(json.withCString { tb_set_extra_scan_paths($0) })
    }

    /// Replace the process-wide registry of extra Claude config directories.
    /// `json` is an array of absolute directory paths; full-replace semantics
    /// — `[]` clears it. Each registered directory is fetched as its own Claude
    /// quota card, using the Keychain item that directory selects.
    ///
    /// Distinct from `setExtraScanPaths`, which takes the expanded
    /// `<dir>/projects` and `<dir>/transcripts` sub-roots and answers which
    /// directories the usage scanner walks. This one answers whose credential
    /// a quota card is fetched with, so it takes the config directories the
    /// user configured — not the scan subset the core accepted.
    public static func setClaudeConfigDirs(json: String) throws -> ClaudeConfigDirsResult {
        try unwrap(json.withCString { tb_set_claude_config_dirs($0) })
    }

    /// Replace the process-wide registry of captured Antigravity accounts.
    /// `json` is `[{"key":"<64 hex>","label":"..."}]`; full-replace semantics,
    /// `[]` clears it. Malformed JSON throws `bridge("invalid_accounts_json")`
    /// and leaves the registry unchanged. Holds no secret.
    public static func setAntigravityAccounts(json: String) throws -> AntigravityAccountsResult {
        try unwrap(json.withCString { tb_set_antigravity_accounts($0) })
    }

    /// Tell the core which captured account agy is signed into and the login
    /// marker it was confirmed under (both opaque here; no secret). Rust
    /// validates them and, when the live marker still matches, fills the
    /// primary Antigravity card from that account's OAuth result instead of
    /// running agy. A nil key or marker clears the binding (NULL). Rust also
    /// clears it on any invalid input and throws `bridge(<fixed code>)`.
    @discardableResult
    public static func setAntigravityBinding(key: String?, marker: String?) throws -> Bool {
        guard let key, let marker else {
            let r: AntigravityBindingResult = try unwrap(tb_set_antigravity_binding(nil))
            return r.bound
        }
        let json = String(
            decoding: try JSONEncoder().encode(["key": key, "marker": marker]), as: UTF8.self)
        let r: AntigravityBindingResult = try unwrap(json.withCString { tb_set_antigravity_binding($0) })
        return r.bound
    }

    /// Copy agy's current Google login into a Syrtis-owned login-keychain
    /// item. Blocking (spawns `security`, reaches Google): never call on the
    /// main thread. A failure throws `bridge(<fixed code>)`; the code names no
    /// account. Does not register the account — the caller adds it to its
    /// list and calls `setAntigravityAccounts`.
    public static func antigravityCapture() throws -> AntigravityCapturedAccount {
        try unwrap(tb_antigravity_capture())
    }

    /// agy's login marker (attributes only, no secret). Blocking (spawns
    /// `security`): never call on the main thread. Throws
    /// `bridge("marker_unavailable")` when the query could not be answered.
    public static func antigravityLoginMarker() throws -> String {
        let result: AntigravityLoginMarker = try unwrap(tb_antigravity_login_marker())
        return result.marker
    }

    /// One automatic capture of agy's current login, skipping `removedKeys`.
    /// Blocking (spawns `security`, may reach Google): never call on the main
    /// thread. A failure throws `bridge(<fixed code>)`, including `paused` and
    /// `not_signed_in`. Does not register the account.
    public static func antigravityAutoCapture(removedKeys: [String]) throws
        -> AntigravityAutoCaptureResult
    {
        let json = (try? JSONEncoder().encode(removedKeys))
            .flatMap { String(data: $0, encoding: .utf8) } ?? "[]"
        return try unwrap(json.withCString { tb_antigravity_auto_capture($0) })
    }

    /// Delete one captured account's keychain item. Blocking (spawns
    /// `security`): never call on the main thread. Never revokes at Google.
    /// Throws `bridge("invalid_key")` or `bridge("keychain_delete_failed")`.
    /// Does not change the registry.
    public static func antigravityRemove(key: String) throws {
        let result: AntigravityRemoved = try unwrap(key.withCString { tb_antigravity_remove($0) })
        guard result.removed else { throw TBCoreError.bridge("keychain_delete_failed") }
    }

    /// Replace the process-wide registry of macOS Keychain consent. `json` is
    /// `{"<public-client-id>": true|false}`; full-replace semantics — `{}`
    /// clears every grant. A wired client absent from the registry is not read
    /// from the Keychain, so macOS cannot raise its authorization dialog for
    /// that client before the app has asked the user itself. `grok-bot` is the
    /// only wired client; Claude reads its own Keychain items ungated, so this
    /// is not a process-wide no-dialog guarantee.
    ///
    /// The core registry is in-memory and starts empty every launch, so the
    /// app owns re-applying the stored answer at startup and after each edit —
    /// see `GrokBotKeychainConsent`. Not calling this at all is the correct
    /// behaviour for a user who has not agreed: the Grok Bot item is left
    /// untouched.
    public static func setKeychainConsent(json: String) throws -> KeychainConsentResult {
        try unwrap(json.withCString { tb_set_keychain_consent($0) })
    }

    /// Configure Cursor desktop sync: `{"enabled","dir","cliTakeoverConfirmed"}`.
    /// In-memory and default off in the core, so the app re-applies it at
    /// launch. Turning it off deletes the synced usage files.
    public static func setCursorSync(json: String) throws -> CursorSyncConfigResult {
        try unwrap(json.withCString { tb_set_cursor_sync($0) })
    }

    /// One Cursor desktop sync. Blocking (SQLite + network, up to 10 min):
    /// never call on the main thread. Single-flight in the core.
    public static func cursorSync(explicit: Bool) throws -> CursorSyncStatus {
        try unwrap(tb_cursor_sync(explicit ? 1 : 0))
    }

    /// OAuth quota cards for codex/claude/antigravity/copilot/grok/grok-bot. Network-bound;
    /// per-provider failures are reported in each snapshot's `error`.
    public static func agentUsage() throws -> AgentUsagePayload {
        let payload = try decodeAgentUsageBoundary(takeBytes(tb_agent_usage())) {
            logAgentUsageBoundaryEvent($0)
        }
        logAgentUsageTransportDiagnostics(payload)
        return payload
    }

    package static func decodeAgentUsageBoundary(
        _ data: Data?,
        onLog: (AgentUsageBoundaryLogEvent) -> Void
    ) throws -> AgentUsagePayload {
        guard let data else {
            onLog(.returnedNull)
            throw TBCoreError.nullPointer
        }
        do {
            return try decodeEnvelope(data)
        } catch let error as TBCoreError {
            onLog(.bridgeFailed)
            throw error
        } catch {
            onLog(.decodeFailed)
            throw error
        }
    }

    private static func logAgentUsageBoundaryEvent(_ event: AgentUsageBoundaryLogEvent) {
        switch event {
        case .returnedNull:
            ffiLog.error("FFI agent usage returned NULL")
        case .bridgeFailed:
            ffiLog.error("FFI agent usage bridge failed")
        case .decodeFailed:
            ffiLog.error("FFI agent usage decode failed")
        }
    }

    private static func logAgentUsageTransportDiagnostics(_ payload: AgentUsagePayload) {
        for entry in agentUsageTransportLogEntries(payload) {
            switch (entry.status, entry.osCode) {
            case let (status?, osCode?):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public) status=\(status, privacy: .public) osCode=\(osCode, privacy: .public)")
            case let (status?, nil):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public) status=\(status, privacy: .public)")
            case let (nil, osCode?):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public) osCode=\(osCode, privacy: .public)")
            case (nil, nil):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public)")
            }
        }
    }

    /// Hermetic checks for the FFI envelope/error contract, surfaced to the
    /// `--selftest` runner (which lives in the Syrtis module and can't reach
    /// these internal symbols). Exercises the error paths `--smoke` never hits on
    /// live data: an `{"ok":false}` must throw `bridge`, a malformed body must
    /// throw rather than crash. Returns `(label, passed)` pairs.
    public static func envelopeContractChecks() -> [(String, Bool)] {
        var out: [(String, Bool)] = []
        func check(_ label: String, _ passed: Bool) { out.append((label, passed)) }

        // ok:true + data → payload returned verbatim.
        do {
            let ok: TokensPerMin = try decodeEnvelope(
                Data(#"{"ok":true,"data":{"tokensPerMin":42.5}}"#.utf8))
            check("ok:true returns data", ok.tokensPerMin == 42.5)
        } catch {
            check("ok:true returns data", false)
        }

        // ok:false → TBCoreError.bridge carrying the err string.
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{"ok":false,"err":"boom"}"#.utf8))
            check("ok:false throws bridge(boom)", false)
        } catch let TBCoreError.bridge(msg) {
            check("ok:false throws bridge(boom)", msg == "boom")
        } catch {
            check("ok:false throws bridge(boom)", false)
        }

        // ok:true but data absent → decoding contract failure, not bridge failure.
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{"ok":true}"#.utf8))
            check("ok:true without data is decode failure", false)
        } catch is DecodingError {
            check("ok:true without data is decode failure", true)
        } catch {
            check("ok:true without data is decode failure", false)
        }

        // Malformed JSON → thrown DecodingError, never a trap.
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{not json"#.utf8))
            check("malformed body throws", false)
        } catch {
            check("malformed body throws", true)
        }

        // The optional path exists so `tb_quota_curve` can answer "bound, but no
        // history yet". Only an explicit null is that answer.
        do {
            let value: TokensPerMin? = try decodeOptionalEnvelope(
                Data(#"{"ok":true,"data":null}"#.utf8))
            check("optional ok:true with null data is absent history", value == nil)
        } catch {
            check("optional ok:true with null data is absent history", false)
        }

        do {
            let value: TokensPerMin? = try decodeOptionalEnvelope(
                Data(#"{"ok":true,"data":{"tokensPerMin":7.5}}"#.utf8))
            check("optional ok:true returns data", value?.tokensPerMin == 7.5)
        } catch {
            check("optional ok:true returns data", false)
        }

        // An omitted key is ABI drift, not an answer. Synthesized Optional
        // decoding maps it to nil exactly like an explicit null, so without the
        // presence check this would read as an empty curve.
        do {
            let _: TokensPerMin? = try decodeOptionalEnvelope(Data(#"{"ok":true}"#.utf8))
            check("optional ok:true without the data key is decode failure", false)
        } catch is DecodingError {
            check("optional ok:true without the data key is decode failure", true)
        } catch {
            check("optional ok:true without the data key is decode failure", false)
        }

        do {
            let _: TokensPerMin? = try decodeOptionalEnvelope(
                Data(#"{"ok":false,"err":"boom"}"#.utf8))
            check("optional ok:false throws bridge(boom)", false)
        } catch let TBCoreError.bridge(msg) {
            check("optional ok:false throws bridge(boom)", msg == "boom")
        } catch {
            check("optional ok:false throws bridge(boom)", false)
        }

        return out
    }

    /// Hermetic decode checks for the `tb_quota_curve` payload. Every rejection
    /// here corresponds to something the Rust producer cannot emit, so accepting
    /// it would mean rendering an ABI drift as a plausible curve.
    public static func quotaCurveContractChecks() -> [(String, Bool)] {
        var out: [(String, Bool)] = []
        func check(_ label: String, _ passed: Bool) { out.append((label, passed)) }

        func point(
            sampledAt: Int64 = 1_000, usedPercent: String = "10.0", resetAt: Int64 = 1_500,
            durationSeconds: Int64 = 1_000, durationSource: String = "contract",
            origin: String = "liveV3", isActiveGroup: String? = "false"
        ) -> String {
            let active = isActiveGroup.map { #","isActiveGroup":\#($0)"# } ?? ""
            return #"{"sampledAt":\#(sampledAt),"usedPercent":\#(usedPercent),"resetAt":\#(resetAt),"#
                + #""durationSeconds":\#(durationSeconds),"durationSource":"\#(durationSource)","#
                + #""origin":"\#(origin)"\#(active)}"#
        }

        func curve(
            points: [String], oldest: Int64 = 1_000, newest: Int64 = 1_000, count: Int = 1,
            activeResetAt: String? = "1500"
        ) -> Data {
            let active = activeResetAt.map { #""activeResetAt":\#($0),"# } ?? ""
            return Data((#"{"points":[\#(points.joined(separator: ","))],"#
                + #""coverage":{"oldestSampledAt":\#(oldest),"newestSampledAt":\#(newest),"#
                + #""sampleCount":\#(count)},"# + active + #""generation":7}"#).utf8)
        }

        func rejects(_ label: String, _ data: Data) {
            do {
                _ = try JSONDecoder().decode(QuotaCurve.self, from: data)
                check(label, false)
            } catch is DecodingError {
                check(label, true)
            } catch {
                check(label, false)
            }
        }

        do {
            let nullActive = try JSONDecoder().decode(
                QuotaCurve.self,
                from: curve(points: [point()], activeResetAt: "null"))
            check("an explicit null activeResetAt is accepted", nullActive.activeResetAt == nil)
        } catch {
            check("an explicit null activeResetAt is accepted", false)
        }

        do {
            let decoded = try JSONDecoder().decode(
                QuotaCurve.self,
                from: curve(
                    points: [point(), point(sampledAt: 1_200, usedPercent: "99.5")],
                    newest: 1_200, count: 2))
            check(
                "a well-formed curve decodes",
                decoded.points.count == 2 && decoded.generation == 7
                    && decoded.points[0].durationSource == .contract
                    && decoded.points[0].origin == .liveV3
                    && decoded.points[1].usedPercent == 99.5)
        } catch {
            check("a well-formed curve decodes", false)
        }

        rejects(
            "an unknown durationSource is rejected",
            curve(points: [point(durationSource: "guessed")]))
        rejects("an unknown origin is rejected", curve(points: [point(origin: "liveV4")]))
        rejects("an empty curve is rejected", curve(points: [], count: 0))
        rejects(
            "a sampleCount disagreeing with points is rejected",
            curve(points: [point()], count: 2))
        rejects(
            "coverage that does not match its points is rejected",
            curve(points: [point()], oldest: 900))
        rejects(
            "points out of sampledAt order are rejected",
            curve(
                points: [point(sampledAt: 1_200), point()],
                oldest: 1_200, newest: 1_000, count: 2))
        // `sampledAt == resetAt` keeps the zero-length cycle self-consistent, so
        // the containment guard passes and only the duration guard can reject
        // this. With any other `sampledAt` the containment check fires first and
        // this case proves nothing about the guard it names.
        rejects(
            "a non-positive durationSeconds is rejected",
            curve(
                points: [point(sampledAt: 1_500, durationSeconds: 0)],
                oldest: 1_500, newest: 1_500))
        // Mirrors `valid_duration`'s full `1...MAX_DURATION_SECONDS` bound. The
        // upper end needs its own case: a duration above the cap still satisfies
        // the positive check and, with a consistent `resetAt`, the containment
        // check too, so nothing else would reject it.
        rejects(
            "a durationSeconds above the storable cap is rejected",
            curve(
                points: [point(
                    sampledAt: 1_000, resetAt: 1_500,
                    durationSeconds: QuotaCurve.validDurationSeconds.upperBound + 1)]))
        rejects(
            "a usedPercent outside [0, 100] is rejected",
            curve(points: [point(usedPercent: "-0.1")]))
        rejects(
            "a usedPercent above 100 is rejected",
            curve(points: [point(usedPercent: "100.1")]))
        // Zero is a READING, not an absence: a fresh window is 0% used, and
        // rejecting it meant no cycle recorded its own start, so the span
        // between the lowest and highest reading understated every cycle by
        // whatever was spent before the app first saw a non-zero number. This
        // guard mirrors the store's admission and had to widen with it.
        do {
            let zero = try JSONDecoder().decode(
                QuotaCurve.self, from: curve(points: [point(usedPercent: "0.0")]))
            check("a usedPercent of exactly 0 is accepted — a fresh window's own start",
                  zero.points.first?.usedPercent == 0)
        } catch {
            check("a usedPercent of exactly 0 is accepted — a fresh window's own start",
                  false)
        }
        // Rejected by JSON number parsing rather than by the `isFinite` guard,
        // which no JSON input can reach. Kept because it pins that a payload
        // cannot smuggle an unrepresentable number past this boundary.
        rejects(
            "a usedPercent outside Double range is rejected",
            curve(points: [point(usedPercent: "1e400")]))
        rejects(
            "a sampledAt outside its own cycle is rejected",
            curve(points: [point(sampledAt: 400)], oldest: 400, newest: 400))
        // Two identical samples pass the count, coverage and ordering checks, so
        // only the repeat check can reject this.
        rejects(
            "a repeated sample is rejected",
            curve(points: [point(), point()], count: 2))
        // Rust always emits the key, so its absence is drift while an explicit
        // null is the real answer for a series with no active cycle. Both cases
        // are asserted, because rejecting the null too would refuse every valid
        // curve for an inactive series.
        rejects(
            "a curve missing the activeResetAt key is rejected",
            curve(points: [point()], activeResetAt: nil))
        // Same rule, one level down. Rust always emits `isActiveGroup`, so an
        // absent key is ABI drift; defaulting it to false would render every
        // point as finished history — the exact misreading the field was added
        // to remove, arriving silently rather than as a decode failure.
        rejects(
            "a curve point missing the isActiveGroup key is rejected",
            curve(points: [point(isActiveGroup: nil)]))

        // The fixture's generation is 7, which is also what the payload claims,
        // so these two cases differ only in what the caller asked for.
        if let decoded = try? JSONDecoder().decode(
            QuotaCurve.self, from: curve(points: [point()]))
        {
            do {
                try requireAnswering(decoded, request: 7)
                check("a curve answering its request is accepted", true)
            } catch {
                check("a curve answering its request is accepted", false)
            }
            do {
                try requireAnswering(decoded, request: 8)
                check("a curve answering another request is rejected", false)
            } catch is TBCoreError {
                check("a curve answering another request is rejected", true)
            } catch {
                check("a curve answering another request is rejected", false)
            }
        } else {
            check("a curve answering its request is accepted", false)
            check("a curve answering another request is rejected", false)
        }

        do {
            try requireAnswering(nil, request: 8)
            check("absent history is not a generation mismatch", true)
        } catch {
            check("absent history is not a generation mismatch", false)
        }

        return out
    }

    /// Hermetic decoder and formatting checks for the source-aware filter
    /// parity payload. This keeps the Swift side independent of private local
    /// session data while pinning the lower-camel wire statuses and bounded
    /// smoke labels.
    public static func filterParityContractChecks() -> [(String, Bool)] {
        var out: [(String, Bool)] = []
        func check(_ label: String, _ passed: Bool) { out.append((label, passed)) }

        let body = Data(
            #"{"ok":true,"data":{"hourly":{"status":"match","unfiltered":{"entryCount":1,"input":10,"output":20,"cacheRead":3,"cacheWrite":4,"reasoning":5,"totalTokens":42,"messageCount":2,"totalCost":0.25},"full":{"entryCount":1,"input":10,"output":20,"cacheRead":3,"cacheWrite":4,"reasoning":5,"totalTokens":42,"messageCount":2,"totalCost":0.25},"delta":{"entryCount":0,"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":0,"messageCount":0,"totalCost":0}},"agents":{"status":"sourceChanged","unfiltered":null,"full":null,"delta":null},"presentClientCount":2}}"#.utf8
        )
        do {
            let payload: FilterParityProbe = try decodeEnvelope(body)
            check(
                "filter parity lower-camel statuses decode",
                payload.hourly.status == .match && payload.agents.status == .sourceChanged
                    && payload.presentClientCount == 2
            )
            check(
                "filter parity bounded smoke labels",
                payload.smokeSummary
                    == "hourly=MATCH entriesΔ=0 tokensΔ=0 messagesΔ=0 costΔ=0.00; agents=SOURCE_CHANGED / SKIP"
            )
            check(
                "filter parity nullable aggregates decode",
                payload.agents.unfiltered == nil && payload.agents.full == nil
                    && payload.agents.delta == nil
            )
        } catch {
            check("filter parity lower-camel statuses decode", false)
            check("filter parity bounded smoke labels", false)
            check("filter parity nullable aggregates decode", false)
        }

        return out
    }
}
