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
    /// each client's token total, largest first, ties by client id. Never on
    /// the wire.
    public var clientParts: [ClientPart]? = nil
    /// True for a row `tabEntries(clientIds:)` keyed by model and provider in a
    /// multi-client tab, merged or not yet: its identity must not depend on
    /// which client it came from, or it would change when a second client
    /// starts using the model. Never on the wire.
    public var tabKeyed = false

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
        Self.fold(entries) { "\($0.client)\u{0}\($0.model)" }.map(\.row)
    }

    /// The rows one tab shows: `modelLevelEntries` of `clientIds`, and when the
    /// tab covers more than one client, one row per model and provider across
    /// them (the Antigravity tab's `antigravity-cli` and `antigravity-extension`
    /// both report `gemini-3.8-flash`/google; maintainer decision 2026-11-08). A
    /// merged row lists every client's token total in `clientParts`; its
    /// `client` is whichever client the engine listed first, so do not label a
    /// merged row (`clientParts != nil`) by it, nor key any `tabKeyed` row by it
    /// (an unmerged one still has exactly one client and may be labelled).
    ///
    /// A row whose own cost is flagged by `implausibleCostRatio` is never
    /// merged: summed with a healthy row of the same model its ratio would
    /// drop under the threshold and the warning would vanish while the
    /// inflated cost stayed on screen.
    ///
    /// The multi-client fold reads the engine's rows, not `modelLevelEntries`:
    /// that joins one client's providers into one row ("nvidia, openai"),
    /// whose totals belong to neither provider and which no other client's
    /// row for either provider would join. So a client that reaches one
    /// model through two providers is two rows in a multi-client tab and one
    /// in its own tab, and the cost check runs on each provider's row there.
    func tabEntries(clientIds: [String]) -> [ModelReportEntry] {
        let allow = Set(clientIds)
        guard allow.count > 1 else { return modelLevelEntries.filter { allow.contains($0.client) } }

        return Self.fold(entries.filter { allow.contains($0.client) }) { entry in
            entry.implausibleCostRatio == nil ? "\(entry.model)\u{0}\(entry.provider)" : nil
        }.map { group in
            var row = group.row
            row.tabKeyed = row.implausibleCostRatio == nil || group.members.count > 1
            guard group.members.count > 1 else { return row }
            row.clientParts = group.members
                .map { ClientPart(client: $0.client, total: $0.total) }
                .sorted { $0.total != $1.total ? $0.total > $1.total : $0.client < $1.client }
            return row
        }
    }

    /// Fold `rows` that share a key into one (`merged`), in first-seen order;
    /// a nil key keeps the row on its own. Each group keeps its members so a
    /// caller can describe what went in.
    private static func fold(
        _ rows: [ModelReportEntry], key: (ModelReportEntry) -> String?
    ) -> [(row: ModelReportEntry, members: [ModelReportEntry])] {
        var indices: [String: Int] = [:]
        var groups: [(row: ModelReportEntry, members: [ModelReportEntry])] = []
        for entry in rows {
            let groupKey = key(entry)
            if let groupKey, let index = indices[groupKey] {
                groups[index].row = merged(groups[index].row, entry)
                groups[index].members.append(entry)
                continue
            }
            if let groupKey { indices[groupKey] = groups.count }
            groups.append((entry, [entry]))
        }
        return groups
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
