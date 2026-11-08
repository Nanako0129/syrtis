import Foundation

// Per-model report (`ModelReport` in types.ts). Note: the wire key for
// throughput is `msPer1kTokens` (serde camelCase of ms_per_1k_tokens) —
// types.ts declares `msPer1KTokens` but the Rust serialization wins.

public struct ModelReportEntry: Decodable, Sendable {
    public let client: String
    public let model: String
    public let provider: String
    public let input: Int64
    public let output: Int64
    public let cacheRead: Int64
    public let cacheWrite: Int64
    public let reasoning: Int64
    public let total: Int64
    public let messageCount: Int
    public let cost: Double
    public let msPer1kTokens: Double?
    /// What the local pricing table would charge for this row's tokens; `nil`
    /// when it cannot price them (no cached dataset, or a model it does not
    /// carry). See `local_cost_estimate` in
    /// crates/tb_core_ffi/src/model_report.rs.
    ///
    /// Optional so a report serialized before this field existed still
    /// decodes.
    public let costEstimate: Double?
    /// Set only on a row `tabEntries(clientIds:)` merged from several clients:
    /// each client's token total, largest first. Never on the wire.
    public var clientParts: [ClientPart]? = nil

    private enum CodingKeys: String, CodingKey {
        case client, model, provider, input, output, cacheRead, cacheWrite, reasoning, total
        case messageCount, cost, msPer1kTokens, costEstimate
    }
}

/// One client's share of a model row merged across a tab's clients.
public struct ClientPart: Sendable, Equatable {
    public let client: String
    public let total: Int64

    public init(client: String, total: Int64) {
        self.client = client
        self.total = total
    }
}

public extension ModelReportEntry {
    /// How many times `cost` exceeds what the local pricing table would
    /// charge, when that multiple is large enough to call the cost broken;
    /// `nil` otherwise.
    ///
    /// A client that reports its own per-message cost (OpenCode, MiMo Code)
    /// has that figure taken verbatim by tokscale-core — it never meets a
    /// pricing table — so a unit error upstream reaches the row unchallenged.
    /// This is the only thing standing between that and the number on screen.
    ///
    /// Computed here rather than in Rust because it must be evaluated AFTER
    /// `modelLevelEntries` folds the provider-split rows together: one
    /// component at 100x merged with a larger healthy row is a merged 1.1x,
    /// and describing the merged cost with the component's ratio would be
    /// materially false.
    ///
    /// One-directional. Rows measured on real data run 0.1-0.3x of the
    /// estimate (the table prices dearer than OpenCode actually bills), and a
    /// genuinely free model reports 0.0 against a positive estimate; a
    /// two-sided check would flag both forever.
    var implausibleCostRatio: Double? {
        guard let costEstimate, costEstimate > 0, cost.isFinite, cost > 0 else { return nil }
        let ratio = cost / costEstimate
        return ratio > CostPlausibility.threshold ? ratio : nil
    }
}

/// Where a self-reported cost stops being expensive and starts being broken.
public enum CostPlausibility {
    /// Measured against real local data, LiteLLM estimate vs OpenCode's own
    /// figure:
    ///
    ///     deepseek-v4-flash (default)          0.7139 /    2.5719 =   0.3x
    ///     deepseek-v4-flash (low)              0.2943 /    1.0470 =   0.3x
    ///     deepseek-v4-pro                      3.9039 /   26.4468 =   0.1x
    ///     deepseek-v4-flash @ exptech (user) 5495.30  /   17.84   = 308x
    ///     deepseek-v4-flash-free    (user)   3174.69  /    9.79   = 324x
    ///
    /// Healthy rows top out near 0.3x, so the threshold is nowhere near 1.0.
    /// 50x leaves two orders of magnitude of headroom above every healthy row
    /// while still catching the reporter's, which matters because the estimate
    /// is only approximate: the lookup can match a near-miss key
    /// (`deepseek-v4-flash-free` resolves to the paid `deepseek-v4-flash`
    /// entry) or a row missing cache rates, each skewing it by single-digit
    /// multiples. Nothing short of a unit-scale error clears 50x.
    public static let threshold: Double = 50

    /// Amber, not red: the row is untrustworthy, not broken — the tokens are
    /// real and the client may yet be right about its own rate.
    public static let warningColor = "#f59e0b"

    /// Shown beside the cost on a flagged row.
    public static let symbol = "exclamationmark.triangle.fill"
}

public struct ModelReport: Decodable, Sendable {
    public let entries: [ModelReportEntry]
    public let totalInput: Int64
    public let totalOutput: Int64
    public let totalCacheRead: Int64
    public let totalCacheWrite: Int64
    public let totalMessages: Int
    public let totalCost: Double
    /// Unix-seconds time the LiteLLM pricing dataset was last fetched
    /// (drives the "Prices updated …" hint). Absent before the first fetch.
    public let pricingUpdatedAt: UInt64?
}

public extension ModelReport {
    /// Fold provider-split rows back to the model-level view used by the
    /// existing cards. Throughput is intentionally dropped when rows merge:
    /// tokscale only computes it honestly over the complete rollup.
    var modelLevelEntries: [ModelReportEntry] {
        var indices: [String: Int] = [:]
        var folded: [ModelReportEntry] = []

        for entry in entries {
            let key = "\(entry.client)\u{0}\(entry.model)"
            guard let index = indices[key] else {
                indices[key] = folded.count
                folded.append(entry)
                continue
            }

            folded[index] = Self.merged(folded[index], entry)
        }
        return folded
    }

    /// The rows one tab shows: `modelLevelEntries` of `clientIds`, and when the
    /// tab covers more than one client, one row per model across them (the
    /// Antigravity tab's `antigravity-cli` and `antigravity-extension` both
    /// report `gemini-3.8-flash`; maintainer decision 2026-11-08). A merged row
    /// keeps the first client's id for its identity and lists every client's
    /// token total in `clientParts`.
    func tabEntries(clientIds: [String]) -> [ModelReportEntry] {
        let allow = Set(clientIds)
        let perClient = modelLevelEntries.filter { allow.contains($0.client) }
        guard allow.count > 1 else { return perClient }

        var indices: [String: Int] = [:]
        var folded: [ModelReportEntry] = []
        var parts: [[ClientPart]] = []
        for entry in perClient {
            let part = ClientPart(client: entry.client, total: entry.total)
            guard let index = indices[entry.model] else {
                indices[entry.model] = folded.count
                folded.append(entry)
                parts.append([part])
                continue
            }
            folded[index] = Self.merged(folded[index], entry)
            parts[index].append(part)
        }
        for index in folded.indices where parts[index].count > 1 {
            folded[index].clientParts = parts[index].sorted { $0.total > $1.total }
        }
        return folded
    }

    /// `current` plus `entry`, keeping `current`'s client and model. Throughput
    /// is dropped: tokscale only computes it honestly over the complete rollup.
    private static func merged(_ current: ModelReportEntry, _ entry: ModelReportEntry) -> ModelReportEntry {
        let input = current.input.saturatingAdding(entry.input)
        let output = current.output.saturatingAdding(entry.output)
        let cacheRead = current.cacheRead.saturatingAdding(entry.cacheRead)
        let cacheWrite = current.cacheWrite.saturatingAdding(entry.cacheWrite)
        let reasoning = current.reasoning.saturatingAdding(entry.reasoning)
        let messageCount = current.messageCount.addingReportingOverflow(entry.messageCount)
        return ModelReportEntry(
            client: current.client,
            model: current.model,
            provider: Self.mergedProviders(current.provider, entry.provider),
            input: input,
            output: output,
            cacheRead: cacheRead,
            cacheWrite: cacheWrite,
            reasoning: reasoning,
            total: input
                .saturatingAdding(output)
                .saturatingAdding(cacheRead)
                .saturatingAdding(cacheWrite)
                .saturatingAdding(reasoning),
            messageCount: messageCount.overflow
                ? Int.max
                : messageCount.partialValue,
            cost: current.cost + entry.cost,
            msPer1kTokens: nil,
            // Sum the estimates alongside the costs, so the ratio taken
            // from the merged row describes the merged cost. All-or-
            // nothing: if any contributing row could not be priced, the
            // sum is a partial denominator that would overstate the ratio
            // for the whole row, so the merged row reports no estimate
            // rather than a misleading one.
            costEstimate: current.costEstimate.flatMap { c in
                entry.costEstimate.map { c + $0 }
            }
        )
    }

    private static func mergedProviders(_ first: String, _ second: String) -> String {
        Set(([first, second].flatMap { $0.components(separatedBy: ", ") }))
            .sorted()
            .joined(separator: ", ")
    }
}
