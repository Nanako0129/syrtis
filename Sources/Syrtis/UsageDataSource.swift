import Foundation
import TokenBarCore

/// App-level source of usage data. Every usage consumer depends on this
/// contract so `--demo` can replace the complete usage surface without
/// allowing live FFI calls to leak into the demo path.
protocol UsageDataSource: Sendable {
    /// Whether this source may read/write the persistent last-good quota cache.
    var allowsQuotaCachePersistence: Bool { get }

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport
    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport
    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport
    func agentUsage() async throws -> AgentUsagePayload
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket]
    func tokensPerMin() async throws -> Double
    /// `accountKey` selects whose transcripts are folded into the window —
    /// nil is the primary account, an extra Claude account passes its config
    /// directory. Required for the same reason `quotaCurve`'s is: a window is
    /// one account's, and a caller that omitted it would measure every
    /// account's usage against one account's allowance.
    func windowUsage(
        accountKey: String?, from: Int64, until: Int64
    ) async throws -> WindowUsage
    /// `accountKey` selects which account of `clientId` to read — nil is the
    /// primary account. Required (not defaulted) so a conformer that forgets
    /// it fails to compile against the protocol instead of silently falling
    /// through to the extension's always-nil default.
    func quotaCurve(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) async throws -> QuotaCurve?
    /// Synchronous because it is a ~2ms read of an already-persisted file, and
    /// because the card's first stage must complete without a task hop — an
    /// await here would put the "instant" half behind the scheduler.
    func quotaCurveSync(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) throws -> QuotaCurve?

    // SA-3d: the device-aware pages. All five BLOCK (a `refresh` runs up to
    // one fetch deadline per selected producer), so the dashboard model calls
    // them on a `Thread` of its own, never on the main actor or the
    // cooperative pool. Cross-source merging is the core's: these return what
    // `tb_*_devices` computed, never a sum Swift made.
    func remotePeers() throws -> [RemotePeer]
    func graphDevices(
        year: String?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<UsagePayload>
    func modelReportDevices(
        year: String?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<ModelReport>
    func hourlyReportDevices(
        year: String?, clients: [String]?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<HourlyReport>
    func agentsReportDevices(
        year: String?, clients: [String]?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<AgentsReport>
}

/// What a source without paired devices answers to the device-aware calls.
enum DeviceSourceError: Error { case unsupported }

extension UsageDataSource {
    /// Defaults that never touch `TBCore`: only `LiveUsageDataSource` reaches
    /// the agent. The demo source and the test doubles list no devices, so the
    /// device menu never appears for them and no real agent is read.
    func remotePeers() throws -> [RemotePeer] { [] }
    func graphDevices(
        year: String?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<UsagePayload> { throw DeviceSourceError.unsupported }
    func modelReportDevices(
        year: String?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<ModelReport> { throw DeviceSourceError.unsupported }
    func hourlyReportDevices(
        year: String?, clients: [String]?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<HourlyReport> { throw DeviceSourceError.unsupported }
    func agentsReportDevices(
        year: String?, clients: [String]?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<AgentsReport> { throw DeviceSourceError.unsupported }
}

extension UsageDataSource {
    /// Defaulted so the pre-existing test doubles keep compiling. They answer
    /// "no window data", which the card renders as its unavailable state —
    /// asserting the card through a double that predates it would test the
    /// double. The live and demo sources both override.
    func windowUsage(
        accountKey: String?, from: Int64, until: Int64
    ) async throws -> WindowUsage {
        WindowUsage(messages: [], undatedCount: 0, processingTimeMs: 0)
    }

    func quotaCurve(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) async throws -> QuotaCurve? { nil }

    func quotaCurveSync(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) throws -> QuotaCurve? { nil }
}

/// The only normal-runtime owner of usage calls into `TBCore`.
/// A floor under how often the OAuth quota endpoints are asked.
///
/// Three independent callers fetch this payload — the popover's poll loop, the
/// Settings window's own poll loop, and the tray's five-minute refresh — and
/// each is written fetch-first: it calls, then sleeps. The popover's loop is
/// mounted by a `.task` that dies with the popover, so every reopen starts the
/// loop again and issues the leading call immediately. Opening and closing the
/// popover ten times issues ten requests, with nothing in between them.
///
/// That is enough to reach Anthropic's rate limit on `oauth/usage`. The engine
/// has a gate for the aftermath — a 429 blocks further attempts until the
/// `Retry-After` passes — but nothing bounded the attempts that produce the
/// 429 in the first place. This is that bound.
///
/// 50 seconds, against a 60-second poll: short enough that the poll itself is
/// never skipped, long enough that reopening cannot outpace it. The five-minute
/// tray refresh is unaffected. One consequence worth knowing: a payload
/// carrying the gate's own "retrying in ~Ns" message is cached like any other,
/// so that countdown can read up to 50s stale.
///
/// Concurrent callers share one request, through a retained in-flight task.
///
/// The actor alone does NOT give that, which is what the first version of this
/// claimed. Swift actors are reentrant across `await`: while the first caller
/// is suspended inside the fetch, the actor is free to admit the second, which
/// finds the cache still empty and starts its own request. Three callers
/// arriving together would issue three — the burst this exists to prevent,
/// surviving inside the thing built to prevent it. Holding the `Task` and
/// awaiting it is what actually coalesces them.
actor AgentUsageThrottle {
    static let shared = AgentUsageThrottle()

    static let minimumInterval: TimeInterval = 50

    private var last: (payload: AgentUsagePayload, at: Date)?
    private var inFlight: Task<AgentUsagePayload, Error>?
    /// Advanced by `invalidate()`. A request compares the value it was issued
    /// under against the current one before caching its result — `Task` is a
    /// struct, so identity comparison is not available for the same purpose.
    private var generation = 0

    /// `now` is a clock rather than an instant, and is read twice: once to
    /// decide, and once to stamp the result.
    ///
    /// Stamping the DECISION time was wrong in exactly the case the floor is
    /// there for. `oauth/usage` is the endpoint that rate-limits, so it is also
    /// the endpoint that goes slow, and a request taking near or past the
    /// 50-second floor returned a payload already older than the floor by the
    /// time it landed. The next reopen — the loop is remounted by a `.task`, so
    /// that is any reopen — then measured freshness from before the request and
    /// issued another immediately. The protection dissolved precisely while the
    /// endpoint was struggling, which is when it was carrying the load.
    ///
    /// Injectable so a test can drive both readings rather than sleep.
    func payload(
        now: @Sendable () -> Date = { Date() },
        fetch: @Sendable @escaping () async throws -> AgentUsagePayload
    ) async throws -> AgentUsagePayload {
        if let last, now().timeIntervalSince(last.at) < Self.minimumInterval {
            return last.payload
        }
        // A request is already out: wait for it rather than opening a second.
        // Its result is this caller's answer — they asked inside one round trip
        // of each other, and the endpoint cannot have moved between them.
        if let inFlight { return try await inFlight.value }
        let issuedAt = generation
        let task = Task { try await fetch() }
        inFlight = task
        defer { if generation == issuedAt { inFlight = nil } }
        let fresh = try await task.value
        // Only cache it if nothing invalidated while it was in flight. Stamping
        // unconditionally would re-seat, for another full window, the answer
        // this request was already too old to give.
        if generation == issuedAt { last = (fresh, now()) }
        return fresh
    }

    /// Forget the cached payload because the QUESTION changed, not because
    /// enough time passed.
    ///
    /// The floor exists to stop repeated identical questions reaching an
    /// endpoint that rate-limits. A Claude account being added or removed is
    /// not that: the cached answer describes a set of accounts that no longer
    /// exists, and holding it for the rest of the 50-second window is what made
    /// a card outlive its account — the delay users saw as "it does update, but
    /// only after a while".
    ///
    /// `inFlight` is dropped as well as `last`. A request already out was
    /// issued against the previous registry, so the next caller must start its
    /// own rather than join one whose answer is about the old accounts. The
    /// outstanding task still completes and still returns to whoever is already
    /// awaiting it; it simply stops being anyone else's answer.
    func invalidate() {
        generation &+= 1
        last = nil
        inFlight = nil
    }

    /// Test seam only: forget the cached payload.
    func reset() { last = nil }
}

struct LiveUsageDataSource: UsageDataSource {
    let allowsQuotaCachePersistence = true

    func windowUsage(
        accountKey: String?, from: Int64, until: Int64
    ) async throws -> WindowUsage {
        try await Task.detached(priority: .userInitiated) {
            try TBCore.windowUsage(accountKey: accountKey, from: from, until: until)
        }.value
    }

    func quotaCurve(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) async throws -> QuotaCurve? {
        try await Task.detached(priority: .userInitiated) {
            try TBCore.quotaCurve(
                clientId: clientId, accountKey: accountKey, windowKey: windowKey,
                generation: generation)
        }.value
    }

    func quotaCurveSync(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) throws -> QuotaCurve? {
        try TBCore.quotaCurve(
            clientId: clientId, accountKey: accountKey, windowKey: windowKey,
            generation: generation)
    }

    func remotePeers() throws -> [RemotePeer] { try TBCore.remotePeers() }

    func graphDevices(
        year: String?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<UsagePayload> {
        try TBCore.graphDevices(year: year, selection: selection, mode: mode)
    }

    func modelReportDevices(
        year: String?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<ModelReport> {
        try TBCore.modelReportDevices(year: year, selection: selection, mode: mode)
    }

    func hourlyReportDevices(
        year: String?, clients: [String]?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<HourlyReport> {
        try TBCore.hourlyReportDevices(
            year: year, clients: clients, selection: selection, mode: mode)
    }

    func agentsReportDevices(
        year: String?, clients: [String]?, selection: DeviceSelection, mode: DeviceFetchMode
    ) throws -> DeviceAwarePayload<AgentsReport> {
        try TBCore.agentsReportDevices(
            year: year, clients: clients, selection: selection, mode: mode)
    }

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        try await Task.detached(priority: priority) {
            try TBCore.graph(year: year)
        }.value
    }

    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        try await Task.detached(priority: priority) {
            try TBCore.refreshGraph(year: year)
        }.value
    }

    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        try await Task.detached(priority: priority) {
            try TBCore.modelReport(year: year)
        }.value
    }

    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport {
        try await Task.detached(priority: priority) {
            try TBCore.hourlyReport(year: year, clients: clients)
        }.value
    }

    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport {
        try await Task.detached(priority: priority) {
            try TBCore.agentsReport(year: year, clients: clients)
        }.value
    }

    func agentUsage() async throws -> AgentUsagePayload {
        try await AgentUsageThrottle.shared.payload {
            try await Task.detached(priority: .utility) {
                try TBCore.agentUsage()
            }.value
        }
    }

    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        try await Task.detached(priority: .utility) {
            try TBCore.usageTrace(windowSecs: windowSecs)
        }.value
    }

    func tokensPerMin() async throws -> Double {
        try await Task.detached(priority: .utility) {
            try TBCore.tokensPerMin()
        }.value
    }
}

/// Synthetic source used by the hidden `--demo` mode. It only exposes the
/// deterministic fixtures in `DemoData`; it has no dependency on `TBCore`.
struct DemoUsageDataSource: UsageDataSource {
    let allowsQuotaCachePersistence = false

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        _ = priority
        return DemoData.payload(for: year)
    }

    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        _ = priority
        // Rebuild rather than cache the value so manual refresh follows the same
        // source boundary as live refresh and keeps the rolling date current.
        return DemoData.payload(for: year)
    }

    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        _ = priority
        return DemoData.modelReport(for: year)
    }

    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport {
        _ = priority
        return DemoData.hourlyReport(for: year, clients: clients)
    }

    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport {
        _ = priority
        return DemoData.agentsReport(for: year, clients: clients)
    }

    func agentUsage() async throws -> AgentUsagePayload {
        DemoData.publishedAgentUsage
    }

    func quotaCurve(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) async throws -> QuotaCurve? {
        try quotaCurveSync(
            clientId: clientId, accountKey: accountKey, windowKey: windowKey,
            generation: generation)
    }

    func quotaCurveSync(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) throws -> QuotaCurve? {
        DemoData.quotaCurve(clientId: clientId, windowKey: windowKey, generation: generation)
    }

    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        DemoData.trace(windowSecs: windowSecs)
    }

    func tokensPerMin() async throws -> Double {
        DemoData.tokensPerMin
    }
}

/// Selects one source for the process lifetime. The mode is intentionally
/// launch-time only; changing the flag requires relaunching the app.
enum UsageDataSources {
    static let current: any UsageDataSource = make(arguments: CommandLine.arguments)

    static func make(arguments: [String]) -> any UsageDataSource {
        arguments.contains("--demo") ? DemoUsageDataSource() : LiveUsageDataSource()
    }
}
