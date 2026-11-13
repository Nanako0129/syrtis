import Foundation
import TokenBarCore

/// Deterministic synthetic usage for the hidden `--demo` mode. The fixture is
/// rebuilt on each source read so its rolling current-year window follows the
/// local day, while every surface still derives from one set of client rows.
enum DemoData {
    static var payload: UsagePayload { payload(for: nil) }
    static var modelReport: ModelReport { modelReport(for: nil) }
    static var hourlyReport: HourlyReport { hourlyReport(for: nil, clients: nil) }
    static var agentsReport: AgentsReport { agentsReport(for: nil, clients: nil) }
    static var agentUsage: AgentUsagePayload { makeAgentUsage() }
    /// What `DemoUsageDataSource` serves: the same payload with a publication
    /// generation, which is what lets the quota lens look its curves up.
    /// `agentUsage` above stays without one, because several selftest doubles
    /// serve it and pair it with curves of their own.
    static var publishedAgentUsage: AgentUsagePayload {
        makeAgentUsage(publicationGeneration: quotaGeneration)
    }
    static let quotaGeneration: UInt64 = 1

    /// Demo mode exists for screenshots, so what it shows must not depend on
    /// which tabs and limits this Mac happens to have hidden. Blanks both
    /// visibility settings in the argument domain — exactly what passing
    /// `-tokenbar.tabs.hidden "" -tokenbar.limits.hidden ""` does — so every
    /// reader, `@AppStorage` included, sees nothing hidden, and nothing is
    /// written to disk.
    ///
    /// The argument domain is process-wide whichever `UserDefaults` instance
    /// sets it, so this is applied once, from `main`, and never from a test.
    static func ignoreLocalVisibility() {
        let defaults = UserDefaults.standard
        defaults.setVolatileDomain(
            visibilityArguments(defaults.volatileDomain(forName: UserDefaults.argumentDomain)),
            forName: UserDefaults.argumentDomain)
    }

    /// The argument domain with both visibility keys blanked and everything
    /// else, `-AppleLanguages` included, kept.
    static func visibilityArguments(_ current: [String: Any]) -> [String: Any] {
        var arguments = current
        arguments[ClientRegistry.tabHiddenKey] = ""
        arguments[ClientRegistry.limitsHiddenKey] = ""
        return arguments
    }

    /// Whether this process runs the override: `main` applies it exactly when
    /// this is true. Settings disables its visibility toggles on it, because
    /// they read the blanked value and a toggle would write "hidden = just
    /// this one" over the list actually stored on disk.
    static let ignoresLocalVisibility = CommandLine.arguments.contains("--demo")
    /// The subscriptions that have recorded history in the demo. A handful,
    /// not every client: the lens is read in screenshots, and a real user has
    /// a few subscriptions rather than one per registered client. Every other
    /// client keeps its "nothing recorded yet" state.
    static let quotaHistoryClients: Set<String> = ["claude", "codex", "copilot", "grok-bot"]
    static var trace: [TraceBucket] { trace(windowSecs: 600) }
    static var tokensPerMin: Double {
        trace(windowSecs: 600).reduce(0) { $0 + $1.tokensPerMin }
    }

    static func payload(
        for year: String?, today: String = Format.todayKey()
    ) -> UsagePayload {
        let fixture = makeFixture(year: year, today: today)
        let json: [String: Any] = [
            "meta": [
                "generatedAt": "\(fixture.end)T12:00:00Z",
                "version": "demo",
                "dateRange": ["start": fixture.start, "end": fixture.end],
            ],
            "summary": [
                "totalTokens": fixture.allTotals.total,
                "totalCost": fixture.allTotals.cost,
                "totalDays": fixture.dates.count,
                "activeDays": fixture.dates.count,
                "averagePerDay": fixture.allTotals.cost / Double(fixture.dates.count),
                "maxCostInSingleDay": fixture.maxDayCost,
                "clients": ClientRegistry.allIds,
                "models": ClientRegistry.allIds.map { "demo-\($0)" },
            ],
            "years": fixture.yearMetadata,
            "contributions": fixture.contributionJSON,
        ]
        return decode(json, as: UsagePayload.self)
    }

    static func modelReport(
        for year: String?, today: String = Format.todayKey()
    ) -> ModelReport {
        let fixture = makeFixture(year: year, today: today)
        let entries = ClientRegistry.allIds.map { id in
            let totals = fixture.clientTotals[id]!
            return [
                "client": id,
                "model": "demo-\(id)",
                "provider": "demo",
                "input": totals.input,
                "output": totals.output,
                "cacheRead": totals.cacheRead,
                "cacheWrite": totals.cacheWrite,
                "reasoning": totals.reasoning,
                "total": totals.total,
                "messageCount": totals.messages,
                "cost": totals.cost,
                "msPer1kTokens": 1.25,
            ] as [String: Any]
        }
        let json: [String: Any] = [
            "entries": entries,
            "totalInput": fixture.allTotals.input,
            "totalOutput": fixture.allTotals.output,
            "totalCacheRead": fixture.allTotals.cacheRead,
            "totalCacheWrite": fixture.allTotals.cacheWrite,
            "totalMessages": fixture.allTotals.messages,
            "totalCost": fixture.allTotals.cost,
        ]
        return decode(json, as: ModelReport.self)
    }

    static func hourlyReport(
        for year: String?, clients: [String]?, today: String = Format.todayKey()
    ) -> HourlyReport {
        let fixture = makeFixture(year: year, today: today)
        let allowed = clientFilter(clients)
        var buckets: [String: HourlyTotals] = [:]
        for (dayIndex, rows) in fixture.rowsByDay.enumerated() {
            for (clientIndex, row) in rows.enumerated() where allowed.contains(row.client) {
                let hour = 8 + ((clientIndex + dayIndex) % 10)
                let key = "\(row.date) \(String(format: "%02d:00", hour))"
                var bucket = buckets[key] ?? HourlyTotals()
                bucket.add(row)
                buckets[key] = bucket
            }
        }

        let entries = buckets.keys.sorted().map { hour in
            let bucket = buckets[hour]!
            return [
                "hour": hour,
                "clients": bucket.clients.sorted(),
                "models": bucket.models.sorted(),
                "input": bucket.input,
                "output": bucket.output,
                "cacheRead": bucket.cacheRead,
                "cacheWrite": bucket.cacheWrite,
                "reasoning": bucket.reasoning,
                "total": bucket.total,
                "messageCount": bucket.messages,
                "turnCount": bucket.turns,
                "cost": bucket.cost,
            ] as [String: Any]
        }
        let totalCost = buckets.values.reduce(0.0) { $0 + $1.cost }
        return decode(
            ["entries": entries, "totalCost": totalCost],
            as: HourlyReport.self)
    }

    static func agentsReport(
        for year: String?, clients: [String]?, today: String = Format.todayKey()
    ) -> AgentsReport {
        let fixture = makeFixture(year: year, today: today)
        let allowed = clientFilter(clients)
        let entries = ClientRegistry.allIds.compactMap { id -> [String: Any]? in
            guard allowed.contains(id), let totals = fixture.clientTotals[id] else { return nil }
            return [
                "agent": "Demo Main · \(id)",
                "clients": [id],
                "input": totals.input,
                "output": totals.output,
                "cacheRead": totals.cacheRead,
                "cacheWrite": totals.cacheWrite,
                "reasoning": totals.reasoning,
                "total": totals.total,
                "cost": totals.cost,
                "messages": totals.messages,
            ] as [String: Any]
        }
        let totalCost = entries.reduce(0.0) { partial, entry in
            partial + (entry["cost"] as? Double ?? 0)
        }
        let totalMessages = entries.reduce(0) { partial, entry in
            partial + (entry["messages"] as? Int ?? 0)
        }
        return decode(
            [
                "entries": entries,
                "totalCost": totalCost,
                "totalMessages": totalMessages,
            ],
            as: AgentsReport.self)
    }

    static func trace(windowSecs: Int64) -> [TraceBucket] {
        _ = windowSecs
        let fixture = makeFixture(year: nil, today: Format.todayKey())
        let buckets = ClientRegistry.allIds.map { id in
            let total = fixture.clientTotals[id]?.total ?? 1
            let tokens = max(1, total / 10)
            return [
                "client": id,
                "agent": "Demo Main",
                "model": "demo-\(id)",
                "tokens": tokens,
                "messages": 1,
                "tokens_per_min": Double(tokens) / 10.0,
            ] as [String: Any]
        }
        return decode(buckets, as: [TraceBucket].self)
    }

    private struct ClientRow {
        let date: String
        let client: String
        let model: String
        let input: Int64
        let output: Int64
        let cacheRead: Int64
        let cacheWrite: Int64
        let reasoning: Int64
        let cost: Double
        let messages: Int

        var total: Int64 {
            input
                .saturatingAdding(output)
                .saturatingAdding(cacheRead)
                .saturatingAdding(cacheWrite)
                .saturatingAdding(reasoning)
        }

        var json: [String: Any] {
            [
                "client": client,
                "modelId": model,
                "providerId": "demo",
                "tokens": [
                    "input": input,
                    "output": output,
                    "cacheRead": cacheRead,
                    "cacheWrite": cacheWrite,
                    "reasoning": reasoning,
                ],
                "cost": cost,
                "messages": messages,
            ]
        }
    }

    private struct Totals {
        var input: Int64 = 0
        var output: Int64 = 0
        var cacheRead: Int64 = 0
        var cacheWrite: Int64 = 0
        var reasoning: Int64 = 0
        var cost = 0.0
        var messages = 0

        var total: Int64 {
            input
                .saturatingAdding(output)
                .saturatingAdding(cacheRead)
                .saturatingAdding(cacheWrite)
                .saturatingAdding(reasoning)
        }

        mutating func add(_ row: ClientRow) {
            input = input.saturatingAdding(row.input)
            output = output.saturatingAdding(row.output)
            cacheRead = cacheRead.saturatingAdding(row.cacheRead)
            cacheWrite = cacheWrite.saturatingAdding(row.cacheWrite)
            reasoning = reasoning.saturatingAdding(row.reasoning)
            cost += row.cost
            messages += row.messages
        }
    }

    private struct HourlyTotals {
        var clients = Set<String>()
        var models = Set<String>()
        var input: Int64 = 0
        var output: Int64 = 0
        var cacheRead: Int64 = 0
        var cacheWrite: Int64 = 0
        var reasoning: Int64 = 0
        var messages = 0
        var turns = 0
        var cost = 0.0

        var total: Int64 {
            input
                .saturatingAdding(output)
                .saturatingAdding(cacheRead)
                .saturatingAdding(cacheWrite)
                .saturatingAdding(reasoning)
        }

        mutating func add(_ row: ClientRow) {
            clients.insert(row.client)
            models.insert(row.model)
            input = input.saturatingAdding(row.input)
            output = output.saturatingAdding(row.output)
            cacheRead = cacheRead.saturatingAdding(row.cacheRead)
            cacheWrite = cacheWrite.saturatingAdding(row.cacheWrite)
            reasoning = reasoning.saturatingAdding(row.reasoning)
            messages += row.messages
            turns += row.messages
            cost += row.cost
        }
    }

    private struct YearTotals {
        var totals = Totals()
        var start: String
        var end: String
    }

    private struct Fixture {
        let dates: [String]
        let start: String
        let end: String
        let rowsByDay: [[ClientRow]]
        let contributionJSON: [[String: Any]]
        let clientTotals: [String: Totals]
        let allTotals: Totals
        let maxDayCost: Double
        let yearMetadata: [[String: Any]]
    }

    private static func makeFixture(year: String?, today: String) -> Fixture {
        let dates = dates(for: year, today: today)
        let ids = ClientRegistry.allIds
        var rowsByDay: [[ClientRow]] = []
        var clientTotals = Dictionary(uniqueKeysWithValues: ids.map { ($0, Totals()) })
        var allTotals = Totals()
        var maxDayCost = 0.0
        var yearTotals: [String: YearTotals] = [:]
        var contributionJSON: [[String: Any]] = []

        for (dayIndex, date) in dates.enumerated() {
            var rows: [ClientRow] = []
            var dayTotals = Totals()
            for (clientIndex, id) in ids.enumerated() {
                let input = Int64(900 + clientIndex * 73 + dayIndex * 31)
                let output = Int64(420 + clientIndex * 29 + dayIndex * 17)
                let cacheRead = Int64(90 + (clientIndex + dayIndex) * 11)
                let cacheWrite = Int64(18 + (clientIndex * 3 + dayIndex) % 19)
                let reasoning = Int64(35 + (clientIndex * 5 + dayIndex) % 23)
                let messages = 2 + (clientIndex + dayIndex) % 3
                let total = input + output + cacheRead + cacheWrite + reasoning
                let cost = Double(total) * 0.000_003
                let row = ClientRow(
                    date: date, client: id, model: "demo-\(id)", input: input,
                    output: output, cacheRead: cacheRead, cacheWrite: cacheWrite,
                    reasoning: reasoning, cost: cost, messages: messages)
                rows.append(row)
                dayTotals.add(row)
                clientTotals[id]!.add(row)
                allTotals.add(row)
            }
            rowsByDay.append(rows)
            maxDayCost = max(maxDayCost, dayTotals.cost)
            let dateYear = String(date.prefix(4))
            if var yearTotal = yearTotals[dateYear] {
                yearTotal.totals.add(rows[0])
                for row in rows.dropFirst() { yearTotal.totals.add(row) }
                yearTotal.end = date
                yearTotals[dateYear] = yearTotal
            } else {
                var yearTotal = YearTotals(totals: Totals(), start: date, end: date)
                for row in rows { yearTotal.totals.add(row) }
                yearTotals[dateYear] = yearTotal
            }
            // Mirror the engine's per-day turn map so the demo dashboard shows
            // the same Daily/Monthly turn column the live one does. The fixture
            // treats every message as a turn, matching how it fills the hourly
            // report's `turnCount`.
            var demoTurns: [String: Int] = [:]
            for row in rows {
                demoTurns[row.client, default: 0] += row.messages
            }
            contributionJSON.append([
                "date": date,
                "turnsByClient": demoTurns,
                "totals": [
                    "tokens": dayTotals.total,
                    "cost": dayTotals.cost,
                    "messages": dayTotals.messages,
                ],
                "intensity": (dayIndex % 4) + 1,
                "tokenBreakdown": [
                    "input": dayTotals.input,
                    "output": dayTotals.output,
                    "cacheRead": dayTotals.cacheRead,
                    "cacheWrite": dayTotals.cacheWrite,
                    "reasoning": dayTotals.reasoning,
                ],
                "clients": rows.map(\.json),
            ])
        }

        let yearMetadata = yearTotals.keys.sorted().map { key in
            let value = yearTotals[key]!
            return [
                "year": key,
                "totalTokens": value.totals.total,
                "totalCost": value.totals.cost,
                "range": ["start": value.start, "end": value.end],
            ] as [String: Any]
        }
        return Fixture(
            dates: dates, start: dates[0], end: dates[dates.count - 1],
            rowsByDay: rowsByDay, contributionJSON: contributionJSON,
            clientTotals: clientTotals, allTotals: allTotals,
            maxDayCost: maxDayCost, yearMetadata: yearMetadata)
    }

    private static func makeAgentUsage(publicationGeneration: UInt64? = nil) -> AgentUsagePayload {
        let now = Date()
        let formatter = ISO8601DateFormatter()
        let updated = formatter.string(from: now)
        let sessionDuration: Int64 = 18_000
        let weeklyDuration: Int64 = 604_800

        func learningHistoryWindow(
            cardId: String, label: String, used: Double, duration: Int64
        ) -> [String: Any] {
            [
                "cardId": cardId,
                "label": label,
                "usedPercent": used,
                "remainingPercent": 100 - used,
                "resetsAt": formatter.string(
                    from: activeReset(now: now, duration: duration)),
                "resetText": duration == sessionDuration ? "in 2h 30m" : "in 3d 12h",
                "windowMinutes": duration / 60,
                "paceStatus": [
                    "state": "learningHistory",
                    "windowKey": cardId,
                    "durationSeconds": duration,
                    "durationSource": "contract",
                    "completeCycles": 0,
                ],
            ]
        }

        let monthlyDuration: Int64 = 2_592_000
        // One quota card per SUBSCRIPTION, not per client id. A client whose
        // `quotaOwner` is someone else draws on that account and has no
        // allowance of its own, so handing it a snapshot here invents a card the
        // real provider never publishes — and since #346 grouped Antigravity's
        // IDE and CLI under one tab, that invented card renders as a second,
        // identical quota row beside the one it borrowed from.
        //
        // The filter sits after `enumerated()` on purpose: the demo percentages
        // are derived from the index, so filtering first would renumber every
        // client after the excluded one and silently change fixtures that have
        // nothing to do with this.
        //
        // Grok is unaffected and must stay that way: Grok Build and Grok Bot
        // share a tab but not an allowance — Build is the local CLI, Bot is the
        // Cursor-billed cloud quota — so `quotaOwner` leaves both owning
        // themselves and both keep a card.
        let agents = ClientRegistry.allIds.enumerated()
            .filter { ClientRegistry.quotaOwner($0.element) == $0.element }
            .map { index, id in
            let sessionUsed = Double(12 + (index * 7) % 76)
            let weeklyUsed = max(5, sessionUsed * 0.58)
            let windows: [[String: Any]]
            if id == "kiro" {
                // The real Kiro provider reports one monthly allowance and no
                // cycle-length evidence, so pace learns the duration
                // (learning-duration). Model that honestly rather than the
                // generic Session/Weekly fixture.
                windows = [
                    [
                        "cardId": "usage.v1",
                        "label": "Monthly",
                        "usedPercent": 41.0,
                        "remainingPercent": 59.0,
                        "resetsAt": formatter.string(
                            from: now.addingTimeInterval(TimeInterval(weeklyDuration))),
                        "resetText": "in 18d",
                        "paceStatus": [
                            "state": "learningDuration",
                            "windowKey": "usage.v1",
                            "durationSource": "observed",
                            "completeCycles": 0,
                        ],
                    ]
                ]
                return [
                    "clientId": id,
                    "source": "fixture",
                    "updatedAt": updated,
                    "identity": ["email": "demo@\(id).local", "plan": "Kiro Pro"],
                    "windows": windows,
                ] as [String: Any]
            }
            // The `droid` client carries the Factory subscription quota: a
            // Standard and a Core tier, each with five-hour, weekly and monthly
            // windows (`agent_factory.rs`). The five-hour and weekly windows carry
            // a contract duration; the monthly one follows the billing cycle, so
            // pace learns it, as the real adapter reports.
            if id == "droid" {
                func monthlyWindow(cardId: String, label: String, used: Double) -> [String: Any] {
                    [
                        "cardId": cardId,
                        "label": label,
                        "usedPercent": used,
                        "remainingPercent": 100 - used,
                        "resetsAt": formatter.string(
                            from: now.addingTimeInterval(TimeInterval(monthlyDuration / 2))),
                        "resetText": "in 14d 6h",
                        "paceStatus": [
                            "state": "learningDuration",
                            "windowKey": cardId,
                            "durationSource": "observed",
                            "completeCycles": 0,
                        ],
                    ]
                }
                windows = [
                    learningHistoryWindow(
                        cardId: "standard.five_hour.v1", label: "Standard 5h", used: 24,
                        duration: sessionDuration),
                    learningHistoryWindow(
                        cardId: "standard.weekly.v1", label: "Standard Weekly", used: 12,
                        duration: weeklyDuration),
                    monthlyWindow(cardId: "standard.monthly.v1", label: "Standard Monthly", used: 17),
                    learningHistoryWindow(
                        cardId: "core.five_hour.v1", label: "Core 5h", used: 6,
                        duration: sessionDuration),
                    learningHistoryWindow(
                        cardId: "core.weekly.v1", label: "Core Weekly", used: 40,
                        duration: weeklyDuration),
                    monthlyWindow(cardId: "core.monthly.v1", label: "Core Monthly", used: 68),
                ]
                return [
                    "clientId": id,
                    "source": "fixture",
                    "updatedAt": updated,
                    "windows": windows,
                ] as [String: Any]
            }            // The `opencode` client carries the OpenCode Go subscription quota:
            // rolling/weekly/monthly percent windows (ported from mana.bar).
            // Give it those three so the demo card matches the real provider
            // shape instead of the generic session/weekly fixture.
            if id == "opencode" {
                // The real OpenCode Go adapter attaches NO duration evidence (the
                // endpoint reports only a percent and a reset), so pace is in the
                // learning-duration state. Model the demo the same way rather than
                // claiming a `contract` duration the provider never supplies.
                func goWindow(
                    cardId: String, label: String, used: Double, duration: Int64,
                    resetText: String
                ) -> [String: Any] {
                    [
                        "cardId": cardId,
                        "label": label,
                        "usedPercent": used,
                        "remainingPercent": 100 - used,
                        "resetsAt": formatter.string(
                            from: now.addingTimeInterval(TimeInterval(duration / 2))),
                        "resetText": resetText,
                        "paceStatus": [
                            "state": "learningDuration",
                            "windowKey": cardId,
                            "durationSource": "observed",
                            "completeCycles": 0,
                        ],
                    ]
                }
                windows = [
                    goWindow(
                        cardId: "rolling.v1", label: "Rolling", used: 47,
                        duration: sessionDuration, resetText: "in 2h 30m"),
                    goWindow(
                        cardId: "weekly.v1", label: "Weekly", used: 63,
                        duration: weeklyDuration, resetText: "in 3d 12h"),
                    goWindow(
                        cardId: "monthly.v1", label: "Monthly", used: 28,
                        duration: monthlyDuration, resetText: "in 14d 6h"),
                ]
                return [
                    "clientId": id,
                    "source": "fixture",
                    "updatedAt": updated,
                    "identity": ["email": "demo@\(id).local", "plan": "Go"],
                    "windows": windows,
                ] as [String: Any]
            }
            switch index {
            case 0:
                windows = [
                    [
                        "cardId": "session.v1",
                        "label": "Session · Learning duration",
                        "usedPercent": 18.0,
                        "remainingPercent": 82.0,
                        "resetsAt": formatter.string(
                            from: activeReset(now: now, duration: sessionDuration)),
                        "resetText": "in 2h 30m",
                        "paceStatus": [
                            "state": "learningDuration",
                            "windowKey": "session.v1",
                            "durationSource": "observed",
                            "completeCycles": 0,
                        ],
                    ],
                    learningHistoryWindow(
                        cardId: "weekly.v1", label: "Weekly · Learning history",
                        used: 35, duration: weeklyDuration),
                ]
            case 1:
                windows = [
                    [
                        "cardId": "session.v1",
                        "label": "Session · Historical ahead",
                        "usedPercent": 72.0,
                        "remainingPercent": 28.0,
                        "resetsAt": formatter.string(
                            from: activeReset(now: now, duration: sessionDuration)),
                        "resetText": "in 2h 30m",
                        "windowMinutes": sessionDuration / 60,
                        "paceStatus": [
                            "state": "available",
                            "windowKey": "session.v1",
                            "durationSeconds": sessionDuration,
                            "durationSource": "contract",
                            "completeCycles": 6,
                        ],
                        "historicalPace": [
                            "expectedUsedPercent": 35.0,
                            "etaSeconds": 1_800.0,
                            "willLastToReset": false,
                            "runOutProbability": 0.42,
                        ],
                    ],
                    [
                        "cardId": "weekly.v1",
                        "label": "Weekly · Typed unavailable",
                        "usedPercent": 20.0,
                        "remainingPercent": 80.0,
                        "resetText": "reset unavailable",
                        "paceStatus": [
                            "state": "unavailable",
                            "windowKey": "weekly.v1",
                            "completeCycles": 0,
                            "reason": "missingReset",
                        ],
                    ],
                ]
            case _ where id == "grok-bot":
                windows = [learningHistoryWindow(
                    cardId: "weekly.v1", label: "Weekly",
                    used: weeklyUsed, duration: weeklyDuration)]
            default:
                windows = [
                    learningHistoryWindow(
                        cardId: "session.v1", label: "Session",
                        used: sessionUsed, duration: sessionDuration),
                    learningHistoryWindow(
                        cardId: "weekly.v1", label: "Weekly",
                        used: weeklyUsed, duration: weeklyDuration),
                ]
            }
            return [
                "clientId": id,
                "source": "fixture",
                "updatedAt": updated,
                "identity": ["email": "demo@\(id).local", "plan": "Demo pace fixture"],
                "windows": windows,
            ] as [String: Any]
        }
        var payload: [String: Any] = [
            "generatedAt": updated,
            "agents": agents,
            "opencodeSubscriptions": ["Codex", "Claude"],
        ]
        payload["publicationGeneration"] = publicationGeneration
        return decode(payload, as: AgentUsagePayload.self)
    }

    /// The running window's reset, shared by the payload and the curves so the
    /// two agree on which cycle is current. Mid-window, as the payload always
    /// was, and on a whole minute so a payload and a curve built a few
    /// milliseconds apart name the same reset.
    static func activeReset(now: Date, duration: Int64) -> Date {
        let raw = Int64(now.timeIntervalSince1970) + duration / 2
        return Date(timeIntervalSince1970: TimeInterval(raw - raw % 60))
    }

    /// Recorded history for the demo's session and weekly windows.
    ///
    /// Consumption follows a working week — weekday office hours heavy,
    /// evenings light, nights and weekends near idle — so the heatmap shows a
    /// rhythm rather than noise. Each completed weekly cycle ends at 30-90%;
    /// a session's total follows how busy its hours were, so office-hour
    /// sessions can run out and idle ones are not recorded at all. The
    /// running cycle ends at the percentage the payload reports, so the curve
    /// and the card agree. Readings are whole percents, as providers report
    /// them. Deterministic per client and window.
    static func quotaCurve(
        clientId: String, windowKey: String, generation: UInt64, now: Date = Date()
    ) -> QuotaCurve? {
        let duration: Int64, cycles: Int, step: Int64
        switch windowKey {
        case "session.v1": (duration, cycles, step) = (18_000, 48, 900)
        case "weekly.v1": (duration, cycles, step) = (604_800, 10, 7_200)
        default: return nil
        }
        guard quotaHistoryClients.contains(clientId),
              let agent = makeAgentUsage().agents.first(where: { $0.clientId == clientId }),
              let current = agent.windows.first(where: { $0.paceStatus.windowKey == windowKey })
        else { return nil }
        let seed = clientId.unicodeScalars.reduce(UInt64(windowKey.count)) {
            $0 &* 31 &+ UInt64($1.value)
        }
        func unit(_ k: Int) -> Double {
            var x = seed &+ UInt64(k) &* 0x9E37_79B9_7F4A_7C15
            x ^= x >> 33; x &*= 0xFF51_AFD7_ED55_8CCD; x ^= x >> 33
            return Double(x % 10_000) / 10_000
        }
        let calendar = Calendar.current
        func rate(_ t: Int64) -> Double {
            let date = Date(timeIntervalSince1970: TimeInterval(t))
            let weekday = calendar.component(.weekday, from: date)  // 1 = Sunday
            let hour = calendar.component(.hour, from: date)
            let day: Double = weekday == 1 ? 0.15 : weekday == 7 ? 0.3 : 1
            let time: Double = switch hour {
            case 9..<12: 1
            case 13..<19: 1.25
            case 12, 19..<23: 0.45
            default: 0.04
            }
            return day * time
        }
        let nowSecs = Int64(now.timeIntervalSince1970)
        let activeResetAt = Int64(activeReset(now: now, duration: duration).timeIntervalSince1970)
        var points: [[String: Any]] = []
        for k in stride(from: cycles, through: 0, by: -1) {
            let resetAt = activeResetAt - Int64(k) * duration
            let start = resetAt - duration
            let end = k == 0 ? nowSecs : resetAt
            var stamps: [Int64] = []
            var t = start + step
            // A completed cycle stops short of its reset: the next cycle starts
            // at that instant, and a reading there would open the running
            // cycle's sparkline on the previous cycle's final value.
            while k == 0 ? t <= end : t < end { stamps.append(t); t += step }
            guard !stamps.isEmpty else { continue }
            // A week always spans the rhythm, so a weekly total can be drawn
            // freely. A five-hour session cannot: its total has to follow how
            // busy those hours were, or a Sunday-night session would consume
            // as much as a Tuesday afternoon.
            let busy = stamps.reduce(0) { $0 + rate($1) } / Double(stamps.count)
            let total: Double = k == 0 ? current.usedPercent
                : duration == 18_000 ? min(100, 105 * busy * (0.6 + 0.6 * unit(k)))
                : 30 + 60 * unit(k)
            // An idle session is never opened, so it has no readings at all.
            if k > 0, total < 3 { continue }
            var cumulative: [Double] = []
            var sum = 0.0
            for stamp in stamps {
                sum += rate(stamp) * (0.6 + 0.8 * unit(Int(stamp / step)))
                cumulative.append(sum)
            }
            for (index, (stamp, c)) in zip(stamps, cumulative).enumerated() {
                // The last reading of a cycle is its total exactly: `total * c / sum`
                // with c == sum can land on 43.4999… and round down, so the running
                // cycle's newest point disagreed with its card (43 vs 44) whenever the
                // local-time rhythm made that division inexact (seen with TZ=UTC).
                let value = index == stamps.count - 1 ? total : total * c / max(sum, 1e-9)
                points.append([
                    "sampledAt": stamp,
                    "usedPercent": min(100, value.rounded()),
                    "resetAt": resetAt,
                    "durationSeconds": duration,
                    "durationSource": "contract",
                    "origin": "liveV3",
                    "isActiveGroup": k == 0,
                ])
            }
        }
        let stamps = points.compactMap { $0["sampledAt"] as? Int64 }
        guard let oldest = stamps.min(), let newest = stamps.max() else { return nil }
        return decode(
            [
                "points": points,
                "coverage": [
                    "oldestSampledAt": oldest, "newestSampledAt": newest,
                    "sampleCount": points.count,
                ],
                "activeResetAt": activeResetAt,
                "generation": generation,
            ] as [String: Any],
            as: QuotaCurve.self)
    }

    /// Returns the synthetic contribution dates for an injectable local today.
    /// All-years keeps the rolling 14-day window, current-year clamps its start
    /// to January 1, and every other valid year stays within that year.
    static func dates(for year: String?, today: String) -> [String] {
        guard let todayDay = ISODay(today) else {
            return dates(for: nil, today: Format.todayKey())
        }
        let currentYear = String(today.prefix(4))
        let end: ISODay
        let validYear = year.flatMap(Self.parseYear)
        if year == nil {
            end = todayDay
        } else if let validYear, String(format: "%04d", validYear) == currentYear {
            end = todayDay
        } else if let validYear, let yearEnd = ISODay("\(validYear)-12-31") {
            end = yearEnd
        } else {
            // Invalid year input falls back to the all-years rolling fixture;
            // DashboardModel still rejects the invalid filter via payload.years.
            end = todayDay
        }

        let start: Int
        if year == nil {
            start = end.number - 13
        } else if let validYear, String(format: "%04d", validYear) == currentYear,
                  let yearStart = ISODay("\(currentYear)-01-01")
        {
            start = max(yearStart.number, end.number - 13)
        } else if let validYear, let yearStart = ISODay("\(validYear)-01-01") {
            start = max(yearStart.number, end.number - 13)
        } else {
            start = end.number - 13
        }
        return (start...end.number).map { ISODay(number: $0).iso }
    }

    private static func parseYear(_ raw: String) -> Int? {
        guard raw.count == 4, let value = Int(raw), (1...9999).contains(value) else {
            return nil
        }
        return value
    }

    private static func clientFilter(_ clients: [String]?) -> Set<String> {
        guard let clients, !clients.isEmpty else { return Set(ClientRegistry.allIds) }
        return Set(clients)
    }

    private static func decode<T: Decodable>(_ json: Any, as type: T.Type) -> T {
        do {
            let data = try JSONSerialization.data(withJSONObject: json)
            return try JSONDecoder().decode(type, from: data)
        } catch {
            preconditionFailure("demo fixture failed to decode \(type): \(error)")
        }
    }
}
