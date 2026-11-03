import Foundation
import TokenBarCore

/// CLI smoke flow (Phase 1): exercise every FFI entry point and print a
/// one-line summary each. Kept behind `--smoke` so CI can validate the
/// bridge without booting the menu-bar app.
enum Smoke {
    /// Runs every check and returns the process exit code (0 = all green).
    /// Per-provider quota errors inside `agentUsage` print as card errors and
    /// do not fail the run; only thrown errors count as failures.
    static func run() -> Int32 {
        var failures = 0

        func summarize(_ label: String, _ body: () throws -> String) {
            do {
                print("\(label): \(try body())")
            } catch {
                failures += 1
                print("\(label): FAILED — \(error)")
            }
        }

        summarize("probe") {
            let probe = try TBCore.probe()
            return "\(probe.messages ?? 0) parsed local messages"
        }

        summarize("graph") {
            let graph = try TBCore.graph()
            return "\(graph.contributions.count) days, total tokens \(graph.summary.totalTokens), "
                + "cost $\(String(format: "%.2f", graph.summary.totalCost)), "
                + "\(graph.summary.clients.count) clients, \(graph.years.count) years"
        }

        summarize("refreshGraph(2026)") {
            let graph = try TBCore.refreshGraph(year: "2026")
            return "\(graph.contributions.count) days, range \(graph.meta.dateRange.start)..\(graph.meta.dateRange.end)"
        }

        summarize("models") {
            let report = try TBCore.modelReport()
            let top = report.entries.max(by: { $0.cost < $1.cost })
            return "\(report.entries.count) models, \(report.totalMessages) messages, "
                + "top=\(top.map { "\($0.model) ($\(String(format: "%.2f", $0.cost)))" } ?? "none"), "
                + "pricesUpdated=\(report.pricingUpdatedAt.map(String.init) ?? "nil")"
        }

        summarize("hourly") {
            let report = try TBCore.hourlyReport()
            return "\(report.entries.count) slots, total cost $\(String(format: "%.2f", report.totalCost))"
        }

        summarize("agents") {
            let report = try TBCore.agentsReport()
            let top = report.entries.first // pre-sorted by cost desc
            return "\(report.entries.count) agents, \(report.totalMessages) messages, "
                + "top=\(top.map(\.agent) ?? "none")"
        }

        // The extra-scan-paths setter is the only FFI entry point whose Swift
        // caller swallows its error (`ExtraScanRoots.apply` reaches the same
        // `try?`, so a bridge failure surfaces as a silently empty result
        // rather than a crash). Exercising it here is what keeps an
        // envelope-field or decoder mismatch from passing every other gate:
        // the Rust tests call `set_from_json` directly and the Swift selftest
        // never touches the FFI.
        // An empty object is safe — the registry is process-local, so clearing
        // it affects nothing beyond this short-lived smoke process.
        summarize("extra-scan-paths") {
            let result = try TBCore.setExtraScanPaths(json: "{}")
            guard result.registeredCount == 0,
                result.unreadable.isEmpty,
                result.rejected.isEmpty
            else {
                throw TBCoreError.bridge(
                    "empty registry decoded as registered=\(result.registeredCount) "
                        + "unreadable=\(result.unreadable.count) rejected=\(result.rejected.count)")
            }
            return "empty registry round-trips (registered 0, unreadable 0, rejected 0)"
        }

        // Install the configured Claude accounts before the quota check below.
        //
        // Unlike the scan-paths registry above, an EMPTY one here would make
        // this smoke describe a process that no user runs: the account registry
        // is what decides how many Claude cards get fetched, so leaving it empty
        // silently tests the single-account path and reports it as coverage of
        // the multi-account one. That is exactly how the isolated-environment
        // check first came back showing one account when two were configured.
        //
        // The shipping app installs this asynchronously at launch
        // (`ExtraScanRoots.apply`), so a fetch issued in the first moments can
        // still race it and see an empty registry; losing that race costs one
        // refresh, and the accounts appear on the next.
        summarize("claude-accounts") {
            let configDirs = ClaudeExtraRoots.load()
            // The payload already leaves out what the core refuses, so the
            // setter must accept all of it; a refusal here means Swift and
            // the core disagree about the rule.
            let accepted = ClaudeExtraRoots.classify(configDirs).accepted
            let result = try TBCore.setClaudeConfigDirs(
                json: ClaudeExtraRoots.configDirsPayloadJSON(accepted: accepted))
            guard result.registeredCount == accepted.count, result.rejected.isEmpty else {
                throw TBCoreError.bridge(
                    "sent \(accepted.count) but registered \(result.registeredCount) "
                        + "with \(result.rejected.count) rejected")
            }
            return "\(result.registeredCount) extra account(s) registered, "
                + "\(configDirs.count - accepted.count) refused"
        }

        // Install the user's real Keychain answer before the quota check below,
        // for the same reason `claude-accounts` sends the real account list: an
        // empty registry here would describe a process no user runs. It would
        // also make the Grok Bot card below report a consent prompt on a
        // machine that granted consent long ago, which reads as a regression.
        //
        // Consequence worth stating: on a machine that HAS granted, this is
        // what lets the quota check reach the Keychain, so smoke can raise the
        // macOS dialog. That is the status quo — the dialog appeared here
        // before this feature existed too — and gating it would mean smoke
        // never exercises the granted path at all.
        //
        // This is also the only gate that decodes the setter's envelope: the
        // Rust tests call `set_from_json` directly and the Swift selftest
        // injects a fake setter, so a field-name or decoder mismatch would pass
        // both and only surface here.
        summarize("grok-bot-keychain-consent") {
            let granted = GrokBotKeychainConsent.answer() == true
            let result = try TBCore.setKeychainConsent(
                json: granted ? GrokBotKeychainConsent.grantedPayload : "{}")
            guard result.grantedCount == (granted ? 1 : 0), result.rejected.isEmpty else {
                throw TBCoreError.bridge(
                    "stored answer granted=\(granted) but registered "
                        + "\(result.grantedCount) with \(result.rejected.count) rejected")
            }
            return granted
                ? "consent granted, Keychain read allowed"
                : "no consent stored, Keychain not read"
        }

        summarize("trace") {
            let buckets = try TBCore.usageTrace(windowSecs: 600)
            let rate = try TBCore.tokensPerMin()
            return "\(buckets.count) buckets (10m window), tokens/min \(String(format: "%.1f", rate))"
        }

        // Drift probe (issue #35): force trayTotals' slow re-sum path over the
        // REAL payload (a hidden id that matches no client excludes nothing) and
        // compare to the FFI summary. Print-only — a mismatch never fails the
        // run; it flags a vendor-sync regression in the aggregator's clamp
        // granularity (see UsagePayload.trayTotals' doc comment).
        summarize("trayDrift") {
            let graph = try TBCore.graph()
            let totals = graph.trayTotals(hidden: ["__none__"], today: Format.todayKey())
            let tokenMatch = totals.totalTokens == graph.summary.totalTokens
            let costMatch = abs(totals.totalCost - graph.summary.totalCost) < 0.01
            let status = tokenMatch && costMatch ? "match" : "MISMATCH"
            return "\(status) — reSum \(totals.totalTokens) tok / "
                + "$\(String(format: "%.2f", totals.totalCost)) vs summary "
                + "\(graph.summary.totalTokens) tok / $\(String(format: "%.2f", graph.summary.totalCost))"
        }

        // Source-aware filter parity (issue #107): one Rust-owned probe uses
        // one LocalSourceContext, a fresh graph, and opaque source tokens around
        // every report. A stable mismatch remains bounded evidence; a changed
        // generation is SOURCE_CHANGED, never a false MISMATCH.
        summarize("filterParity") {
            try TBCore.filterParityProbe().smokeSummary
        }

        // The generation the run's own publication bound its series under. The
        // quota-curve check below needs it: with a publication behind it, any
        // other value is rejected at the generation check and never reaches the
        // binding lookup that check exists to exercise.
        var publishedGeneration: UInt64?

        summarize("agentUsage") {
            let usage = try TBCore.agentUsage()
            publishedGeneration = usage.publicationGeneration
            let cards = usage.agents.map { snapshot in
                if let error = snapshot.error {
                    return "\(snapshot.clientId)=error(\(error))"
                }
                return "\(snapshot.clientId)=\(snapshot.uniqueCardWindows.count) windows"
            }
            let subs = usage.opencodeSubscriptions ?? []
            return cards.joined(separator: ", ")
                + (subs.isEmpty ? "" : " | opencode subs: \(subs.joined(separator: ", "))")
        }

        // tb_quota_curve has no unconditional success path to smoke: it serves a
        // curve only for a series this process bound and that already has stored
        // history, neither of which a CI machine has. What the gate can still
        // prove is that the symbol links, the header signature matches, and the
        // binding table this run just published rejects a series it does not
        // contain.
        //
        // It must use that published generation. Passing anything else makes
        // Rust fail the generation check first, so the binding lookup — the only
        // part of the new boundary this check can reach — never runs, and a
        // broken lookup would still look green. For the same reason the expected
        // error is compared exactly: accepting any bridge error cannot tell the
        // two failures apart.
        summarize("quotaCurve") {
            guard let generation = publishedGeneration else {
                throw SmokeExpectationFailure(
                    "agentUsage published no generation, so no binding table exists to probe")
            }
            do {
                let curve = try TBCore.quotaCurve(
                    clientId: "__smoke__", accountKey: nil, windowKey: "__smoke__", generation: generation)
                throw SmokeExpectationFailure(
                    "unbound series returned \(curve == nil ? "null" : "a curve") instead of failing closed")
            } catch let TBCoreError.bridge(message) {
                guard message == "quota curve binding is unavailable" else {
                    throw SmokeExpectationFailure(
                        "unbound series failed with \"\(message)\" instead of an unavailable binding")
                }
                return "generation \(generation) rejects an unbound series — \(message)"
            }
        }

        if failures > 0 {
            print("\(failures) entry point(s) failed")
            return 1
        }
        return 0
    }
}

/// A smoke check that reached an outcome the contract forbids. Distinct from a
/// thrown FFI error so the printed line cannot be mistaken for one.
private struct SmokeExpectationFailure: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}
