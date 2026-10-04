import Foundation

// OAuth quota cards (`AgentUsagePayload` in the Tauri frontend's
// src/lib/agentUsage.ts).

private let legacyPacePresentationID = "legacy.missing.v1"
private let maxPaceDurationSeconds: Int64 = 400 * 86_400

private func paceDataCorrupted(_ decoder: Decoder, _ message: String) -> DecodingError {
    .dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: message))
}

public struct AgentIdentity: Decodable, Sendable {
    public let email: String?
    public let plan: String?
}

/// A backend-owned historical projection for one quota window.
///
/// The values are produced together by the Rust evaluator. Swift may use the
/// expected usage to classify the current pace, but must preserve the backend's
/// projection (ETA, lasts-to-reset decision, and optional risk) as one result.
public struct HistoricalPace: Decodable, Sendable {
    public let expectedUsedPercent: Double
    public let etaSeconds: Double?
    public let willLastToReset: Bool
    public let runOutProbability: Double?

    public init(
        expectedUsedPercent: Double,
        etaSeconds: Double? = nil,
        willLastToReset: Bool,
        runOutProbability: Double? = nil
    ) {
        precondition(
            Self.validationError(
                expectedUsedPercent: expectedUsedPercent,
                etaSeconds: etaSeconds,
                willLastToReset: willLastToReset,
                runOutProbability: runOutProbability
            ) == nil,
            "invalid HistoricalPace"
        )
        self.expectedUsedPercent = expectedUsedPercent
        self.etaSeconds = etaSeconds
        self.willLastToReset = willLastToReset
        self.runOutProbability = runOutProbability
    }

    private enum CodingKeys: String, CodingKey {
        case expectedUsedPercent, etaSeconds, willLastToReset, runOutProbability
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let expected = try container.decode(Double.self, forKey: .expectedUsedPercent)
        let eta = try container.decodeIfPresent(Double.self, forKey: .etaSeconds)
        let willLast = try container.decode(Bool.self, forKey: .willLastToReset)
        let probability = try container.decodeIfPresent(Double.self, forKey: .runOutProbability)

        if let message = Self.validationError(
            expectedUsedPercent: expected,
            etaSeconds: eta,
            willLastToReset: willLast,
            runOutProbability: probability
        ) {
            throw paceDataCorrupted(decoder, message)
        }

        self.expectedUsedPercent = expected
        self.etaSeconds = eta
        self.willLastToReset = willLast
        self.runOutProbability = probability
    }

    private static func validationError(
        expectedUsedPercent: Double,
        etaSeconds: Double?,
        willLastToReset: Bool,
        runOutProbability: Double?
    ) -> String? {
        guard expectedUsedPercent.isFinite, (0...100).contains(expectedUsedPercent) else {
            return "historical expectedUsedPercent is out of range"
        }
        if let etaSeconds, (!etaSeconds.isFinite || etaSeconds < 0) {
            return "historical etaSeconds is invalid"
        }
        if let runOutProbability, (!runOutProbability.isFinite || !(0...1).contains(runOutProbability)) {
            return "historical runOutProbability is invalid"
        }
        guard (etaSeconds == nil) == willLastToReset else {
            return "historical etaSeconds and willLastToReset contradict"
        }
        return nil
    }
}

public enum UsagePaceState: String, Decodable, Sendable, Equatable {
    case learningDuration
    case learningHistory
    case available
    case unavailable
    /// Internal marker used only when the complete `paceStatus` key is absent.
    case legacyMissing

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        guard let value = Self(rawValue: raw), value != .legacyMissing else {
            throw paceDataCorrupted(decoder, "unknown or internal pace state")
        }
        self = value
    }
}

public enum UsagePaceDurationSource: String, Decodable, Sendable, Equatable {
    case provider
    case contract
    case observed
}

public enum UsagePaceUnavailableReason: String, Decodable, Sendable, Equatable {
    case windowIdentity
    case missingReset
    case invalidEvidence
    case accountScope
    case storeCapacity
    case history
    case nonRecurring
}

/// The typed Rust v3 pace status nested inside one quota window.
public struct PaceStatus: Decodable, Sendable, Equatable {
    public let state: UsagePaceState
    public let windowKey: String?
    public let durationSeconds: Int64?
    public let durationSource: UsagePaceDurationSource?
    public let completeCycles: Int
    public let reason: UsagePaceUnavailableReason?

    /// The key to read this window's quota curve with, or nil when the engine
    /// records no history for it. `accountScope` is the engine's mark for an
    /// account with no trusted history identity (`enrich_snapshot_with`): the
    /// `agy` CLI route of Antigravity, or a Grok Bot token with no subject.
    /// Such a window is neither recorded nor bound, so its curve read throws
    /// "binding is unavailable" on every publication, and reading it anyway
    /// reported a permanent absence as a failed read that "will be retried".
    /// A storage failure is reported as `history` instead and is still read.
    public var historyKey: String? {
        state == .unavailable && reason == .accountScope ? nil : windowKey
    }

    public init(
        state: UsagePaceState,
        windowKey: String? = nil,
        durationSeconds: Int64? = nil,
        durationSource: UsagePaceDurationSource? = nil,
        completeCycles: Int = 0,
        reason: UsagePaceUnavailableReason? = nil
    ) {
        precondition(
            Self.validationError(
                state: state,
                windowKey: windowKey,
                durationSeconds: durationSeconds,
                durationSource: durationSource,
                completeCycles: completeCycles,
                reason: reason
            ) == nil,
            "invalid PaceStatus"
        )
        self.state = state
        self.windowKey = windowKey
        self.durationSeconds = durationSeconds
        self.durationSource = durationSource
        self.completeCycles = completeCycles
        self.reason = reason
    }

    public static let legacyMissing = PaceStatus(
        state: .legacyMissing,
        completeCycles: 0
    )

    private enum CodingKeys: String, CodingKey {
        case state, windowKey, durationSeconds, durationSource, completeCycles, reason
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let state = try container.decode(UsagePaceState.self, forKey: .state)
        let windowKey = try container.decodeIfPresent(String.self, forKey: .windowKey)
        let duration = try container.decodeIfPresent(Int64.self, forKey: .durationSeconds)
        let source = try container.decodeIfPresent(
            UsagePaceDurationSource.self, forKey: .durationSource)
        let completeCycles = try container.decode(Int.self, forKey: .completeCycles)
        let reason = try container.decodeIfPresent(
            UsagePaceUnavailableReason.self, forKey: .reason)

        if let message = Self.validationError(
            state: state,
            windowKey: windowKey,
            durationSeconds: duration,
            durationSource: source,
            completeCycles: completeCycles,
            reason: reason
        ) {
            throw paceDataCorrupted(decoder, message)
        }

        self.state = state
        self.windowKey = windowKey
        self.durationSeconds = duration
        self.durationSource = source
        self.completeCycles = completeCycles
        self.reason = reason
    }

    private static func validationError(
        state: UsagePaceState,
        windowKey: String?,
        durationSeconds: Int64?,
        durationSource: UsagePaceDurationSource?,
        completeCycles: Int,
        reason: UsagePaceUnavailableReason?
    ) -> String? {
        if state == .legacyMissing {
            return (windowKey == nil && durationSeconds == nil && durationSource == nil
                && completeCycles == 0 && reason == nil) ? nil : "legacy pace status has fields"
        }
        guard completeCycles >= 0 else { return "pace completeCycles must be non-negative" }

        let identityUnavailable = state == .unavailable && reason == .windowIdentity
        if (windowKey == nil) != identityUnavailable {
            return "pace windowKey identity invariant failed"
        }
        if let windowKey, windowKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return "pace windowKey must be non-empty"
        }

        if let durationSeconds {
            guard durationSeconds > 0, durationSeconds <= maxPaceDurationSeconds else {
                return "pace durationSeconds is out of range"
            }
            guard durationSource != nil else {
                return "pace durationSource is required with durationSeconds"
            }
        } else if durationSource != nil
                    && !(state == .learningDuration && durationSource == .observed) {
            return "pace durationSource requires a duration"
        }

        switch state {
        case .learningDuration:
            guard durationSeconds == nil, reason == nil else {
                return "learningDuration pace invariant failed"
            }
        case .learningHistory:
            guard durationSeconds != nil, durationSource != nil, reason == nil else {
                return "learningHistory pace invariant failed"
            }
        case .available:
            guard durationSeconds != nil, durationSource != nil, reason == nil else {
                return "available pace invariant failed"
            }
        case .unavailable:
            guard reason != nil else { return "unavailable pace requires a reason" }
            if durationSeconds == nil, durationSource != nil {
                return "unavailable durationSource requires a duration"
            }
        case .legacyMissing:
            return "legacy pace status is not a v3 wire state"
        }
        if state != .unavailable, reason != nil {
            return "non-unavailable pace cannot have a reason"
        }
        return nil
    }
}

public struct UsageWindow: Decodable, Sendable {
    public let cardId: String
    /// The provider's own name for this allowance. Presentation only — never
    /// an identity, and not unique on its own: see
    /// `AgentUsageSnapshot.uniqueCardWindows`, the one place allowed to
    /// qualify it, which is why the setter is file-private rather than `let`.
    public fileprivate(set) var label: String
    public let usedPercent: Double
    public let remainingPercent: Double
    public let resetsAt: String?
    public let resetText: String?
    /// Legacy compatibility only. V3 pace calculations use `durationSeconds`.
    public let windowMinutes: Int64?
    /// Exact v3 quota-window duration. Never inferred from legacy `windowMinutes`.
    public let durationSeconds: Int64?
    /// Typed v3 pace state, or the internal marker for an absent whole key.
    public let paceStatus: PaceStatus
    /// Backend-owned learned projection, present when completed-history or
    /// validated current-cycle evidence passes the v3 quality gate.
    /// Missing or null is state-dependent in the v3 contract.
    public let historicalPace: HistoricalPace?
    /// The model this window's allowance is scoped to, as the provider's own
    /// display-name slug — `fable` for a "Fable only" weekly limit. Nil for
    /// every window the provider did not narrow.
    ///
    /// Emitted by the engine only where the provider DECLARES a scope
    /// (`limits[].scope.model`). It is never inferred from a label here or
    /// there: "Designs" and "Daily Routines" are narrow windows whose scope is
    /// not a model, and a flat `seven_day_opus` field says nothing about scope
    /// at all.
    public let modelScope: String?

    // Defaults preserve existing pure Swift linear fixtures. A v3 status is
    // validated below; the legacy default deliberately does not derive a
    // duration from windowMinutes.
    public init(
        label: String, usedPercent: Double, remainingPercent: Double,
        resetsAt: String? = nil, resetText: String? = nil,
        windowMinutes: Int64? = nil, historicalPace: HistoricalPace? = nil,
        cardId: String? = nil, durationSeconds: Int64? = nil,
        paceStatus: PaceStatus = .legacyMissing,
        modelScope: String? = nil
    ) {
        self.modelScope = modelScope
        precondition(
            Self.usagePercentageValidationError(
                usedPercent: usedPercent,
                remainingPercent: remainingPercent
            ) == nil,
            "invalid UsageWindow percentages"
        )
        let resolvedCardId = cardId ?? legacyPacePresentationID
        if paceStatus.state == .legacyMissing {
            precondition(durationSeconds == nil, "legacy pace cannot carry durationSeconds")
        } else {
            precondition(cardId != nil && !resolvedCardId.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                        "v3 pace requires a non-empty cardId")
            let resolvedDuration = durationSeconds ?? paceStatus.durationSeconds
            precondition(resolvedDuration == paceStatus.durationSeconds,
                        "top-level and nested durationSeconds differ")
            precondition(Self.v3ValidationError(
                paceStatus: paceStatus,
                windowMinutes: windowMinutes,
                durationSeconds: resolvedDuration,
                historicalPace: historicalPace
            ) == nil, "invalid UsageWindow pace invariants")
            self.durationSeconds = resolvedDuration
            self.cardId = resolvedCardId
            self.label = label
            self.usedPercent = usedPercent
            self.remainingPercent = remainingPercent
            self.resetsAt = resetsAt
            self.resetText = resetText
            self.windowMinutes = windowMinutes
            self.paceStatus = paceStatus
            self.historicalPace = historicalPace
            return
        }

        self.cardId = resolvedCardId
        self.label = label
        self.usedPercent = usedPercent
        self.remainingPercent = remainingPercent
        self.resetsAt = resetsAt
        self.resetText = resetText
        self.windowMinutes = windowMinutes
        self.durationSeconds = nil
        self.paceStatus = .legacyMissing
        self.historicalPace = historicalPace
    }

    private enum CodingKeys: String, CodingKey {
        case cardId, label, usedPercent, remainingPercent, resetsAt, resetText
        case windowMinutes, paceStatus, historicalPace, modelScope
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let label = try container.decode(String.self, forKey: .label)
        let usedPercent = try container.decode(Double.self, forKey: .usedPercent)
        let remainingPercent = try container.decode(Double.self, forKey: .remainingPercent)
        let resetsAt = try container.decodeIfPresent(String.self, forKey: .resetsAt)
        let resetText = try container.decodeIfPresent(String.self, forKey: .resetText)
        let windowMinutes = try container.decodeIfPresent(Int64.self, forKey: .windowMinutes)
        let historicalPace = try container.decodeIfPresent(HistoricalPace.self, forKey: .historicalPace)
        // Absent is the ordinary answer — the engine omits the key for every
        // unscoped window — so `decodeIfPresent` is correct here and is NOT the
        // conflation the other optional fields on this type warn about.
        self.modelScope = try container.decodeIfPresent(String.self, forKey: .modelScope)

        if let message = Self.usagePercentageValidationError(
            usedPercent: usedPercent,
            remainingPercent: remainingPercent
        ) {
            throw paceDataCorrupted(decoder, message)
        }

        if container.contains(.paceStatus) {
            let cardId = try container.decode(String.self, forKey: .cardId)
            guard !cardId.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                throw paceDataCorrupted(decoder, "v3 pace requires a non-empty cardId")
            }
            // `decode`, not `decodeIfPresent`, intentionally makes null fail.
            let paceStatus = try container.decode(PaceStatus.self, forKey: .paceStatus)
            if let message = Self.v3ValidationError(
                paceStatus: paceStatus,
                windowMinutes: windowMinutes,
                durationSeconds: paceStatus.durationSeconds,
                historicalPace: historicalPace
            ) {
                throw paceDataCorrupted(decoder, message)
            }
            self.cardId = cardId
            self.label = label
            self.usedPercent = usedPercent
            self.remainingPercent = remainingPercent
            self.resetsAt = resetsAt
            self.resetText = resetText
            self.windowMinutes = windowMinutes
            self.durationSeconds = paceStatus.durationSeconds
            self.paceStatus = paceStatus
            self.historicalPace = historicalPace
            return
        }

        let cardId: String?
        if container.contains(.cardId) {
            cardId = try container.decode(String.self, forKey: .cardId)
            if cardId?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty == true {
                throw paceDataCorrupted(decoder, "legacy pace cardId must be non-empty")
            }
        } else {
            cardId = nil
        }
        self.cardId = cardId ?? legacyPacePresentationID
        self.label = label
        self.usedPercent = usedPercent
        self.remainingPercent = remainingPercent
        self.resetsAt = resetsAt
        self.resetText = resetText
        self.windowMinutes = windowMinutes
        self.durationSeconds = nil
        self.paceStatus = .legacyMissing
        self.historicalPace = historicalPace
    }

    private static func usagePercentageValidationError(
        usedPercent: Double,
        remainingPercent: Double
    ) -> String? {
        guard usedPercent.isFinite, remainingPercent.isFinite,
              (0...100).contains(usedPercent), (0...100).contains(remainingPercent) else {
            return "usage percentages are out of range"
        }
        guard abs(usedPercent + remainingPercent - 100) < 0.000_001 else {
            return "usage percentages must sum to 100"
        }
        return nil
    }

    private static func v3ValidationError(
        paceStatus: PaceStatus,
        windowMinutes: Int64?,
        durationSeconds: Int64?,
        historicalPace: HistoricalPace?
    ) -> String? {
        if let durationSeconds {
            guard windowMinutes == durationSeconds / 60 else {
                return "pace windowMinutes must derive from durationSeconds"
            }
        } else if windowMinutes != nil {
            return "pace windowMinutes requires durationSeconds"
        }

        switch paceStatus.state {
        case .available:
            guard let durationSeconds, durationSeconds > 0, historicalPace != nil else {
                return "available pace requires duration and historicalPace"
            }
        case .learningHistory:
            guard let durationSeconds, durationSeconds > 0, historicalPace == nil else {
                return "learningHistory pace invariant failed"
            }
        case .learningDuration:
            guard durationSeconds == nil, historicalPace == nil else {
                return "learningDuration pace invariant failed"
            }
        case .unavailable:
            guard historicalPace == nil else {
                return "unavailable pace cannot carry historicalPace"
            }
        case .legacyMissing:
            return "legacy pace status cannot appear in v3 wire"
        }
        return nil
    }
}

public struct CreditsSnapshot: Decodable, Sendable {
    public let remaining: Double?
    public let unlimited: Bool
}

public struct AgentUsageTransportDiagnostic: Decodable, Sendable {
    public let category: String?
    public let status: Int64?
    public let osCode: Int64?

    private enum CodingKeys: String, CodingKey {
        case category, status, osCode
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.category = try? container.decode(String.self, forKey: .category)
        self.status = try? container.decode(Int64.self, forKey: .status)
        self.osCode = try? container.decode(Int64.self, forKey: .osCode)
    }
}

/// The instructions an unconfigured card shows. A value rather than a view so
/// the choice is assertable — see `AgentUsageSnapshot.setupInstructions`.
public enum SetupInstructions: Equatable, Sendable {
    /// Claude's setup-token / Keychain instructions, which name Claude-only
    /// environment and Keychain identifiers and belong to no other provider.
    case claudeSetupToken
    /// The provider's own one-line instruction, as it arrived in `error`.
    case providerMessage(String)
    /// Not an unconfigured card, or one with nothing to say.
    case none
}

public struct AgentUsageSnapshot: Decodable, Sendable {
    public let clientId: String
    /// Which account of `clientId` this card is. Absent from the payload — and
    /// so `nil` here — for the primary, which is every account until an extra
    /// Claude config directory is configured. The value is that directory.
    public let accountKey: String?
    public let source: String
    public let updatedAt: String
    public let identity: AgentIdentity?
    public let windows: [UsageWindow]
    public let credits: CreditsSnapshot?
    public let error: String?
    public let transportDiagnostic: AgentUsageTransportDiagnostic?
    /// Antigravity primary on the agy route only: agy's login-item date read
    /// just before this card was fetched. Display-only (Antigravity dedup).
    public let agyLoginMarker: String?
    /// Swift-only, never decoded: the captured account whose recorded history
    /// this card's windows answer from. Set only by `adoptingHistory(of:)`
    /// (Antigravity dedup merging the agy-route primary with its captured
    /// account). Every identity, key and storage slot stays on `accountKey`;
    /// only the account handed to a curve read is `historyReadAccountKey`.
    public private(set) var historyAccountKey: String?
    /// The account to read this card's quota curves under.
    public var historyReadAccountKey: String? { historyAccountKey ?? accountKey }

    private enum CodingKeys: String, CodingKey {
        case clientId, accountKey, source, updatedAt, identity, windows, credits, error,
            transportDiagnostic, agyLoginMarker
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        self.clientId = try container.decode(String.self, forKey: .clientId)
        self.accountKey = try container.decodeIfPresent(String.self, forKey: .accountKey)
        self.source = try container.decode(String.self, forKey: .source)
        self.updatedAt = try container.decode(String.self, forKey: .updatedAt)
        self.identity = try container.decodeIfPresent(AgentIdentity.self, forKey: .identity)
        self.windows = try container.decode([UsageWindow].self, forKey: .windows)
        self.credits = try container.decodeIfPresent(CreditsSnapshot.self, forKey: .credits)
        self.error = try container.decodeIfPresent(String.self, forKey: .error)
        self.transportDiagnostic = try? container.decode(
            AgentUsageTransportDiagnostic.self, forKey: .transportDiagnostic)
        self.agyLoginMarker = try container.decodeIfPresent(String.self, forKey: .agyLoginMarker)
    }

    /// Backend `source` values that mean "this card is waiting on the user",
    /// not "this card failed". Both arrive as a terminal provider failure with
    /// a non-nil `error` — `source` is the only field separating them, which is
    /// why every consumer that distinguishes them has to check it BEFORE it
    /// checks `error`.
    ///
    /// `unconfigured`: no credential exists at all (Claude's setup prompt).
    /// `keychain-consent`: a credential exists and works, but reading it would
    /// raise a macOS authorization dialog the user has not agreed to yet.
    /// `keychain-denied`: the user agreed, but macOS did not grant access —
    /// they pressed Deny, or left the dialog unanswered. Also a prompt rather
    /// than a malfunction: nothing is broken, the permission simply is not
    /// there, and the card offers to ask again.
    /// Enumerated by `setupBadgeKey`, which is the single place that decides
    /// both membership and what the badge says — see its doc for why they are
    /// not two lists.

    /// Whether this card is a prompt for the user rather than a malfunction.
    /// Named once here because the answer is stated at three call sites, and
    /// a fourth `source ==` literal added later would silently render a prompt
    /// as a red error.
    public var isSetupPlaceholder: Bool {
        setupBadgeKey != nil
    }

    /// The localization key the status badge shows for a placeholder card, or
    /// `nil` when this card is not one.
    ///
    /// Lives here, beside the source list, rather than as a ternary at the
    /// badge. Adding `keychain-denied` to the set while leaving the badge
    /// matching only `keychain-consent` is exactly what happened once: the
    /// refused card correctly stopped being an error and then read "Set up",
    /// which is wrong twice over — the login IS set up, and the action is to
    /// retry authorization. Deciding it here means a new source cannot be
    /// half-added; it has to answer this.
    ///
    /// "Allow" is the badge's own key and names a STATE. The button uses the
    /// separate `consent.action.allow`, because a language that distinguishes
    /// state from action cannot serve both from one entry.
    public var setupBadgeKey: String? {
        switch source {
        case "unconfigured": "Set up"
        case "keychain-consent", "keychain-denied": "Allow"
        default: nil
        }
    }

    /// What an unconfigured card offers as instructions.
    ///
    /// Here rather than as a branch in the card's view body, for the reason
    /// `setupBadgeKey` gives one paragraph up. Claude's copy names
    /// `CLAUDE_CODE_OAUTH_TOKEN` and a `tokenbar-claude-oauth-token` Keychain
    /// item, and neither exists for any other provider — but Claude was the only
    /// client that could report `unconfigured` when that copy was written, so the
    /// view showed it unconditionally. Codex and Antigravity now report it too
    /// (#345), and a branch living in a `ViewBuilder` is one nothing can assert:
    /// the next provider to reach this state would have inherited Claude's
    /// Keychain instructions with no test to notice.
    ///
    /// Every other provider already states its own one-line instruction in
    /// `error` ("Run `codex` to log in", "Re-login in Antigravity"), so it says
    /// that instead of borrowing Claude's.
    public var setupInstructions: SetupInstructions {
        guard source == "unconfigured" else { return .none }
        if clientId == "claude" { return .claudeSetupToken }
        guard let error, !error.isEmpty else { return .none }
        return .providerMessage(error)
    }

    /// Order-preserving card view shared by quota resolvers and consumers.
    /// A duplicate card ID is fail-closed after the first occurrence; labels
    /// never repair or disambiguate a card collision.
    ///
    /// Repeated LABELS are the opposite case and are repaired here. Codex
    /// reports its Spark allowance as one additional limit carrying both a
    /// primary and a secondary window — 5 hours and 7 days — and the engine
    /// names both of them `Codex Spark` because the label is the limit's, not
    /// the window's. The two rows are different windows with different card
    /// IDs, reset schedules and histories, so every surface that draws the
    /// label alone offered two identical, indistinguishable choices (#286).
    ///
    /// The qualifier is appended HERE rather than at the six surfaces that
    /// render a window name, and rather than in the engine: this is the one
    /// view every one of them already goes through, the raw `windows` array
    /// keeps the wire label byte-for-byte for the cross-check harness, and no
    /// card ID, window key or persisted selection changes.
    public var uniqueCardWindows: [UsageWindow] {
        Self.qualifyingRepeatedLabels(rawCardWindows)
    }

    /// The same card view with the provider's labels exactly as they arrived.
    ///
    /// For the pre-v3 label migration, and only for it. That migration accepts
    /// a persisted label only when ONE window carries it, and qualification
    /// can break a tie it is supposed to refuse: two windows sharing a label
    /// where only one has duration evidence — a sibling still in
    /// `learningDuration` — leave exactly one raw label behind, so a persisted
    /// label that matched both before would now migrate to whichever window
    /// happened to lack a duration. Ambiguity is a property of what the
    /// provider sent, so it has to be read from what the provider sent.
    public var rawCardWindows: [UsageWindow] {
        var seen = Set<String>()
        return windows.filter { seen.insert($0.cardId).inserted }
    }

    /// Appends a period to a label that another window in the same card view
    /// also carries. A label that appears once is returned untouched, so every
    /// existing single-window presentation is unchanged.
    ///
    /// Three sources of evidence, tried in order, because the FIRST one can be
    /// withdrawn by the provider at exactly the moment the rows are otherwise
    /// indistinguishable:
    ///
    /// 1. `durationSeconds` — the window's own length, named in the app's
    ///    vocabulary (`Session`, `Weekly`, or the span).
    /// 2. `resetsAt` — the span until this row's own reset. A Codex window
    ///    with no usage yet resolves to `unavailable(invalidEvidence)`, and
    ///    `UsageWindow.unavailable` clears the duration AND `windowMinutes`,
    ///    so a pair reported at 100% remaining — the state issue #286 was
    ///    filed from — carries no length at all. The countdown is what the row
    ///    already displays, and it is true by construction rather than a
    ///    period inferred from one.
    /// 3. Position in the card view — a deterministic ordinal. Reached only
    ///    when a group has neither lengths nor resets that separate it, and
    ///    present because #286 requires that unusable duration evidence still
    ///    yields a unique name rather than the identical pair it reports.
    ///
    /// A tier is taken only when it names EVERY window of the group and names
    /// them all differently; two windows of one period would otherwise be
    /// handed a distinction that is not there.
    static func qualifyingRepeatedLabels(
        _ windows: [UsageWindow], now: Date = Date()
    ) -> [UsageWindow] {
        var counts: [String: Int] = [:]
        for window in windows { counts[window.label, default: 0] += 1 }
        guard counts.values.contains(where: { $0 > 1 }) else { return windows }

        // What a window that is NOT being qualified will render as. A generated
        // name has to avoid these too: a snapshot holding two `Foo` windows and
        // one already labelled `Foo · 1` would otherwise be given a second
        // `Foo · 1`, and uniqueness inside the group says nothing about that.
        let untouched = Set(
            windows.filter { counts[$0.label, default: 0] == 1 }.map(\.label))

        func compose(_ label: String, _ qualifier: String) -> String {
            "%@ · %@".localized(label.localized, qualifier)
        }

        func tier(_ candidate: (UsageWindow) -> String?) -> [String: [String]] {
            var byLabel: [String: [String]] = [:]
            for window in windows where counts[window.label, default: 0] > 1 {
                guard let value = candidate(window) else {
                    byLabel[window.label] = []
                    continue
                }
                if byLabel[window.label]?.isEmpty == true { continue }
                byLabel[window.label, default: []].append(value)
            }
            return byLabel.filter { label, values in
                values.count == counts[label] && Set(values).count == values.count
                    && values.allSatisfy { !untouched.contains(compose(label, $0)) }
            }
        }

        let byLength = tier { window in
            window.durationSeconds.flatMap { $0 > 0 ? windowPeriod($0) : nil }
        }
        // The SAME span the countdown in the row prints, rounding included:
        // `durationText` alone rounds to the nearest minute while the countdown
        // takes minutes up, which put `4h 59m` in a name beside `Resets in 5h`
        // in the same row for the first half of every minute.
        let byReset = tier { window in
            window.resetsAt
                .flatMap(parseRFC3339)
                .flatMap { UsagePace.spanText(until: $0, now: now) }
        }

        // The occurrence index within its own repeated-label group: the
        // position each tier's candidates were collected at, and the seed for
        // the ordinal the last tier falls back to. The ordinal walks forward
        // past anything already on screen, so it is unique against the whole
        // output rather than only against its own group.
        var taken: [String: Int] = [:]
        var used = untouched
        return windows.map { window in
            guard counts[window.label, default: 0] > 1 else { return window }
            let index = taken[window.label, default: 0]
            taken[window.label] = index + 1
            var name = (byLength[window.label]?[index] ?? byReset[window.label]?[index])
                .map { compose(window.label, $0) }
            if name == nil || used.contains(name!) {
                var ordinal = index + 1
                while used.contains(compose(window.label, String(ordinal))) {
                    ordinal += 1
                }
                name = compose(window.label, String(ordinal))
            }
            used.insert(name!)
            var qualified = window
            qualified.label = name!
            return qualified
        }
    }

    /// What kind of window this is, in the vocabulary the app already uses.
    ///
    /// The engine names Codex's MAIN rate limit from exactly these two lengths
    /// — `18_000 => "Session"`, `604_800 => "Weekly"` in `codex_windows` — and
    /// deliberately keys on the length rather than on which slot carried it.
    /// A Spark allowance arrives in the same two shapes, so it reads with the
    /// same two words rather than in a second vocabulary of its own; both are
    /// already translated. Any other length falls back to the span itself,
    /// through the same formatter the reset countdown uses — deliberately not
    /// a truncation to the largest unit, which would render one hour and
    /// ninety minutes identically and rebuild the ambiguity being removed.
    private static func windowPeriod(_ seconds: Int64) -> String {
        switch seconds {
        case 18_000: return "Session".localized
        case 604_800: return "Weekly".localized
        default: return UsagePace.durationText(Double(seconds))
        }
    }
}

public struct AgentUsagePayload: Decodable, Sendable {
    public let generatedAt: String
    /// Rust publication order. Older/demo payloads omit this additive field.
    public let publicationGeneration: UInt64?
    public let agents: [AgentUsageSnapshot]
    /// Subscription-type providers opencode is authed against (e.g. ["Codex"]).
    /// Omitted from the JSON entirely when empty.
    public let opencodeSubscriptions: [String]?

    /// Configured quota sources also belong in navigation without session logs.
    /// Error-only snapshots stay reachable; setup placeholders do not add tabs.
    public var configuredClientIds: [String] {
        var seen = Set<String>()
        return agents.filter { !$0.isSetupPlaceholder }.map(\.clientId)
            .filter { seen.insert($0).inserted }
    }
}

package struct AgentUsageTransportLogEntry: Equatable, Sendable {
    package let clientId: String
    package let category: String
    package let status: Int?
    package let osCode: Int32?
}

/// Payload of `tb_quota_provider_ids`: `{"ids": [...]}`.
public struct QuotaProviderIds: Decodable, Sendable {
    public let ids: [String]
}

/// The client ids whose transport diagnostics keep their name in the log; any
/// other id is written as "unknown". Derived from the engine's provider table
/// rather than listed here, so a new provider is attributable the moment it is
/// registered (#324). If the engine cannot answer, the set is empty and every
/// id logs as "unknown": the same fail-closed treatment an unlisted id always
/// had, never a raw id that was not vetted.
private let agentUsageTransportLogClientIds: Set<String> = {
    // A failed call is already logged by `TBCore.unwrap`; an empty success is
    // not, and it would silently anonymize every provider for the process.
    let ids = Set((try? TBCore.quotaProviderIds()) ?? [])
    if ids.isEmpty {
        ffiLog.error("quota provider ids unavailable; transport diagnostics log as unknown")
    }
    return ids
}()

private let agentUsageTransportLogCategories: Set<String> = [
    "timeout", "dns", "tls", "connectionRefused", "connectionReset",
    "connect", "request", "responseBody", "rateLimited", "serverError",
]

package func agentUsageTransportLogEntries(
    _ payload: AgentUsagePayload
) -> [AgentUsageTransportLogEntry] {
    payload.agents.compactMap { snapshot in
        guard let diagnostic = snapshot.transportDiagnostic,
              let rawCategory = diagnostic.category else { return nil }
        let clientId = agentUsageTransportLogClientIds.contains(snapshot.clientId)
            ? snapshot.clientId : "unknown"
        let isKnownCategory = agentUsageTransportLogCategories.contains(rawCategory)
        let category = isKnownCategory ? rawCategory : "unknown"
        let status = diagnostic.status.flatMap { value -> Int? in
            switch rawCategory {
            case "rateLimited":
                return value == 429 ? Int(exactly: value) : nil
            case "serverError":
                return (500...599).contains(value) ? Int(exactly: value) : nil
            default:
                return nil
            }
        }
        let osCode = isKnownCategory && rawCategory != "rateLimited"
            && rawCategory != "serverError"
            ? diagnostic.osCode.flatMap { Int32(exactly: $0) }
            : nil
        return AgentUsageTransportLogEntry(
            clientId: clientId,
            category: category,
            status: status,
            osCode: osCode
        )
    }
}

// Copies for the app's display-only rewrites (Antigravity dedup). Every other
// field is carried over unchanged.
extension AgentUsageSnapshot {
    package func replacingIdentity(_ identity: AgentIdentity?) -> AgentUsageSnapshot {
        AgentUsageSnapshot(copying: self, identity: identity)
    }

    /// The agy-route primary merged with captured account `captured`: this
    /// card keeps its own windows and values (usage, reset, label), takes
    /// `captured`'s pace status, historical pace and duration per matching card
    /// id (so its windows carry history keys), and records `captured` as the account to read curves under.
    ///
    /// Only when `captured` has no error and offers windows; otherwise this
    /// card is returned untouched. A window `captured` lacks keeps its own pace
    /// status. Never traps: see `UsageWindow.replacingPace(from:)`.
    package func adoptingHistory(of captured: AgentUsageSnapshot) -> AgentUsageSnapshot {
        guard captured.error == nil, let key = captured.accountKey,
              !captured.windows.isEmpty
        else { return self }
        let theirs = captured.rawCardWindows
        let merged = windows.map { window in
            theirs.first { $0.cardId == window.cardId }
                .map { window.replacingPace(from: $0) } ?? window
        }
        return AgentUsageSnapshot(
            copying: self, identity: identity, windows: merged, historyAccountKey: key)
    }

    /// Captured account `self` standing in for an errored primary (Antigravity
    /// dedup): the primary slot (no account key), its own identity and
    /// windows, curves still read under its own key.
    package func promotedToPrimary() -> AgentUsageSnapshot {
        AgentUsageSnapshot(copying: self, identity: identity, promoted: true)
    }

    private init(
        copying other: AgentUsageSnapshot, identity: AgentIdentity?,
        windows: [UsageWindow]? = nil, historyAccountKey: String? = nil, promoted: Bool = false
    ) {
        clientId = other.clientId
        accountKey = promoted ? nil : other.accountKey
        source = other.source
        updatedAt = other.updatedAt
        self.identity = identity
        self.windows = windows ?? other.windows
        credits = other.credits
        error = other.error
        transportDiagnostic = other.transportDiagnostic
        agyLoginMarker = other.agyLoginMarker
        self.historyAccountKey = historyAccountKey
            ?? (promoted ? other.accountKey : other.historyAccountKey)
    }
}

extension UsageWindow {
    /// This window with `other`'s pace status AND historical pace (they are one
    /// backend result and travel together) and the duration they describe;
    /// usage, reset and label are kept.
    ///
    /// The duration guard: this window's own duration must be absent or equal
    /// `other`'s. Absent is the real shape of the window this exists for: the
    /// engine's `unavailable("accountScope")` clears `durationSeconds` and
    /// `windowMinutes` (agent_usage.rs `unavailable`), so a primary that
    /// required equality would never be merged. A different duration is a
    /// different cycle length, whose pace and history must not be borrowed.
    ///
    /// Returns `self` unchanged unless the result passes the same pace
    /// validation the initializers apply. They `precondition` those
    /// invariants, so building the copy directly could trap on a payload that
    /// merely disagrees with itself; this never does.
    package func replacingPace(from other: UsageWindow) -> UsageWindow {
        guard durationSeconds == nil || durationSeconds == other.durationSeconds,
              other.durationSeconds == other.paceStatus.durationSeconds,
              !cardId.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              Self.v3ValidationError(
                  paceStatus: other.paceStatus, windowMinutes: other.windowMinutes,
                  durationSeconds: other.durationSeconds,
                  historicalPace: other.historicalPace) == nil
        else { return self }
        return UsageWindow(
            copying: self, paceStatus: other.paceStatus, historicalPace: other.historicalPace,
            durationSeconds: other.durationSeconds, windowMinutes: other.windowMinutes)
    }

    private init(
        copying other: UsageWindow, paceStatus: PaceStatus, historicalPace: HistoricalPace?,
        durationSeconds: Int64?, windowMinutes: Int64?
    ) {
        cardId = other.cardId
        label = other.label
        usedPercent = other.usedPercent
        remainingPercent = other.remainingPercent
        resetsAt = other.resetsAt
        resetText = other.resetText
        self.windowMinutes = windowMinutes
        self.durationSeconds = durationSeconds
        self.paceStatus = paceStatus
        self.historicalPace = historicalPace
        modelScope = other.modelScope
    }
}

extension AgentIdentity {
    package static func make(email: String?, plan: String?) -> AgentIdentity {
        AgentIdentity(email: email, plan: plan)
    }
}

extension AgentUsagePayload {
    package func replacingAgents(_ agents: [AgentUsageSnapshot]) -> AgentUsagePayload {
        AgentUsagePayload(
            generatedAt: generatedAt, publicationGeneration: publicationGeneration,
            agents: agents, opencodeSubscriptions: opencodeSubscriptions)
    }
}
