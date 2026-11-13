import Foundation
import Observation
import TokenBarCore

/// The set of analysis lenses, echoing tokscale's TUI tabs. The client tab
/// (Overview/Claude/Codex…, later phase) filters *which* data; this picks
/// *how* it is broken down. The two compose.
enum AppView: String, CaseIterable {
    /// `quota` sits second because it answers the question `overview` raises:
    /// the summary says which subscription is tightest, this lens is where that
    /// subscription's window and its past windows live. Position is product
    /// order, not an implementation detail — the tab row renders declaration
    /// order, and `SelfTest` pins it.
    case overview, quota, models, monthly, daily, hourly, stats, agents

    /// Title-cased id, then looked up: the English label doubles as the
    /// translation key, while `rawValue` stays the persisted id.
    var label: String { (rawValue.prefix(1).uppercased() + rawValue.dropFirst()).localized }

    /// Lenses the user can individually hide via Settings. Overview and
    /// Models are fixed anchors — Overview is the fallback target for every
    /// hidden lens (see `effective`), so it can never itself be hidden.
    static let toggleable: [AppView] = allCases.filter { $0 != .overview && $0 != .models }

    /// Lenses shown in the tab row, given the persisted hidden-set raw
    /// string. Same comma-separated-ids shape `ClientRegistry` uses for
    /// hidden client tabs — `ClientRegistry.parseIdSet` is reused verbatim,
    /// it's a generic CSV-id parser, not client-specific in implementation.
    /// Only `toggleable` lenses can ever actually be hidden — even a
    /// tampered raw string (e.g. a manually edited UserDefaults value)
    /// can't hide Overview or Models, since Overview must always remain the
    /// guaranteed fallback target (see `effective`).
    static func visible(hiddenRaw: String) -> [AppView] {
        let hidden = ClientRegistry.parseIdSet(hiddenRaw)
        return allCases.filter { !toggleable.contains($0) || !hidden.contains($0.rawValue) }
    }

    /// The view to actually render/label this frame. A hidden lens never
    /// survives — not even for the one frame before `resetViewIfHidden()`
    /// persists the correction — because a transient popover can reopen with
    /// a brand-new view instance whose `onChange` has nothing to compare
    /// against (see StatusItemController's `.transient` behavior). Same
    /// defensive shape as `lensContent`'s inline `singleClient` check for a
    /// just-hidden client tab. Guarded to `toggleable` lenses for the same
    /// tamper-resistance reason as `visible`.
    static func effective(_ view: AppView, hiddenRaw: String) -> AppView {
        guard toggleable.contains(view) else { return view }
        return ClientRegistry.parseIdSet(hiddenRaw).contains(view.rawValue) ? .overview : view
    }
}

/// Value-only apply guard for generated agent-usage payloads. Legacy/demo
/// payloads omit the generation and pass through without changing this state.
struct AgentUsagePublicationState {
    private var latestGeneration: UInt64?
    private var latestPayload: AgentUsagePayload?
    /// The account set (Claude config dirs + Antigravity registry) in force
    /// when `latestPayload`'s generation was first accepted. Stamped only on a
    /// strictly newer generation: an equal re-accept in the window between a
    /// registry change and its invalidation must not give an old payload the
    /// new set.
    private var latestAccountSet: String?

    var latest: AgentUsagePayload? { latestPayload }

    /// The latest payload, only when it was accepted under `accountSet`.
    func latest(forAccountSet accountSet: String) -> AgentUsagePayload? {
        latestAccountSet == accountSet ? latestPayload : nil
    }

    mutating func resolve(_ candidate: AgentUsagePayload, accountSet: String) -> AgentUsagePayload {
        guard let generation = candidate.publicationGeneration else { return candidate }
        if let latestGeneration, generation < latestGeneration {
            return latestPayload ?? candidate
        }
        if latestGeneration.map({ generation > $0 }) ?? true {
            latestAccountSet = accountSet
        }
        latestGeneration = generation
        latestPayload = candidate
        return candidate
    }
}

/// Process-lifetime MainActor state shared by every UI consumer, including the
/// dashboard models and the independent tray poller.
///
/// `resolve` is also where a refused Keychain grant is withdrawn, because this
/// is the one place every payload passes through. Doing it in the card would
/// miss the case that matters most: with the popover closed, the tray keeps
/// polling every five minutes, and a grant left standing after the user
/// pressed Deny would reopen the macOS dialog on that schedule with nothing on
/// screen to explain it.
@MainActor
enum AgentUsagePublicationCoordinator {
    private static var state = AgentUsagePublicationState()

    /// Both accessors return the payload after `AntigravityDedup`, so every
    /// consumer — the popover (`resolve`) and the tray, the status item and
    /// the individual client trays (`latestPayload`, via
    /// `TrayAnimator.publishedQuota`) — draws agy's current account once. The
    /// stored state keeps the raw payload: the dedup depends on
    /// `currentAgyKey` at the moment of reading, not at publication.
    static var latestPayload: AgentUsagePayload? { state.latest.map(antigravityDedup) }

    /// Seed for a freshly built popover model: the last published payload, but
    /// only if the Claude config-dir set and the Antigravity registry are the
    /// ones it was accepted under (a removed account must not reappear).
    static func latestPayload(forAccountSet accountSet: String) -> AgentUsagePayload? {
        state.latest(forAccountSet: accountSet).map(antigravityDedup)
    }

    /// Test seam: the Antigravity registry as the account-set fingerprint
    /// reads it. Put back by `resetForTesting()`.
    static var antigravityRegistryProvider: () -> String = defaultAntigravityRegistry

    private static let defaultAntigravityRegistry: () -> String = {
        AntigravityAccounts.payloadJSON(AntigravityAccounts.load())
    }

    static var accountSet: String {
        ClaudeExtraRoots.appliedConfigDirsJSON + "\n" + antigravityRegistryProvider()
    }

    static func resolve(_ candidate: AgentUsagePayload) -> AgentUsagePayload {
        let resolved = state.resolve(candidate, accountSet: accountSet)
        GrokBotKeychainConsent.revokeIfAccessWasDenied(resolved)
        return antigravityDedup(resolved)
    }

    private static func antigravityDedup(_ payload: AgentUsagePayload) -> AgentUsagePayload {
        AntigravityDedup.apply(
            payload, currentAgyKey: AntigravityAutoCapture.shared.currentAgyKey,
            currentAgyMarker: AntigravityAutoCapture.shared.currentAgyMarker)
    }

    /// Test seam only: back to the state of a process that has published
    /// nothing.
    ///
    /// This floor is process-wide and monotone, so a case that publishes a high
    /// generation silently rejects every later case's payload and hands it the
    /// earlier one instead — the later case then measures a fixture it never
    /// built, and fails for a reason nothing in it points at. That is not
    /// hypothetical: `M3-p` needs generations above every other fixture's to be
    /// published at all, and without this reset it broke `M3-f`, two hundred
    /// lines further down, by doing so.
    static func resetForTesting() {
        state = AgentUsagePublicationState()
        antigravityRegistryProvider = defaultAntigravityRegistry
    }
}

/// What a graph commit actually did. `GraphFetchOutcome.committed` used to be
/// reported unconditionally, even when `apply()`'s empty-year branch left the
/// filter cleared and a reload spawned without committing anything — so a
/// caller reading `.committed` as "the slice settled" was wrong exactly then.
enum ApplyResult: Equatable {
    /// The payload committed; `payload`/`stats`/`acceptedPayloadYear` moved.
    case applied
    /// The selected year was absent from this payload; the filter cleared and
    /// an unfiltered reload was spawned. Nothing committed here.
    case redirectedToAllYears
    /// The expected-year guard failed (a stale fetch for a year the model has
    /// since moved away from). Nothing committed, nothing spawned.
    case rejected
}

/// Which trigger owns a background graph fetch, for the header's freshness
/// indicator. `.manual` deliberately renders through the EXISTING
/// `refreshButton` spinner instead of a second one — see
/// `PopoverView.header`/`refreshButton`.
enum RefreshKind: Equatable {
    case initial, poll, yearSwitch, manual
}

/// A graph fetch currently in flight, keyed on the same `graphFetchToken`
/// ownership rule `commit`/`graphFetchFailed` already follow: only the fetch
/// that owns the token it was created with may clear it, so an overtaken
/// fetch's completion can never clear a NEWER request's indicator.
struct BackgroundRefresh: Equatable {
    let token: Int
    let kind: RefreshKind
}

/// Retained once a restore (memory or disk) leaves the dashboard showing data
/// that has not yet been confirmed current by a live fetch — restored numbers
/// with nothing on screen saying so are just stale numbers. Cleared only by
/// the first ACCEPTED commit (`ApplyResult.applied`); a redirect or a
/// rejection settles nothing, so the age stays displayed.
struct RestoredSnapshot: Equatable {
    let savedAt: Date
    var failed: Bool
}

/// Snapshot of the model's essential state, captured on each successful
/// load so a fresh DashboardModel can start in `.ready` state instead of
/// flashing "Loading usage…" every time the popover reopens.
///
/// `hourly` is excluded because it has its own year/client-keyed process cache;
/// `agents` remains uncached. `agentUsage`/`trace` are NOT lazy lenses — their
/// pollers (`pollAgentUsage`/`pollTrace`) fetch-first and overwrite
/// unconditionally — so caching them is staleness-free and keeps the Overview
/// tab's live/quota cards populated on reopen instead of flashing placeholders.
private struct DashboardSnapshot {
    let payload: UsagePayload
    let stats: UsageStats
    /// When `apply()` committed this `payload` — NOT when this struct itself
    /// was built, so `publishModel()`/`refreshSnapshotLiveData()` republishing
    /// the cache for an unrelated reason (a model landing, a trace poll) never
    /// makes the graph look freshly captured. Feeds the header's restored-age
    /// indicator on a same-process reopen.
    let payloadCapturedAt: Date
    let modelReport: ModelReport?
    /// The payload generation the cached `modelReport` was fetched for. It can
    /// legitimately lag `payload.meta.generatedAt`: a poll may commit a newer
    /// graph while the user sits on a lens that does not show models. Storing
    /// the model's own generation keeps the restore honest, so a lagging report
    /// is re-requested instead of being mistaken for current.
    let modelGeneratedAt: String?
    /// When this process fetched `modelReport`, so a reopen does not reset
    /// the hourly pricing refresh in `ensureModelReport` (#191).
    let modelFetchedAt: Date?
    let colors: ModelColorMap
    let knownYears: [String]
    let year: String?
    let agentUsage: AgentUsagePayload?
    let trace: [TraceBucket]
    /// The two history cards as last published.
    ///
    /// Everything else in this struct exists so a reopen does not repay work
    /// the process already did; these two were the exception, and it showed.
    /// `payload`, `stats` and `agentUsage` restore instantly, so the rest of
    /// the lens is drawn while "Past windows" and "When the allowance goes"
    /// still spin — they start from `[]` on every rebuilt model and have to
    /// re-read every window's curve and re-fold its cycles first (131 of them
    /// on the install that prompted this).
    ///
    /// `qualifyingCycles` is deliberately NOT carried. It is not drawn; it
    /// bounds the message scan, and restoring a stale range would start a scan
    /// over the wrong span to save a redraw nobody sees.
    ///
    /// `quotaHistory` and `quotaEquivalences` ARE carried although they are
    /// scan-derived, because they are drawn and they are the slowest of the
    /// four cards to arrive. That is safe only because
    /// `invalidateScanDerivedCaches()` already drops `lastSnapshot` along with
    /// the union scan and the hourly fold — so a scan-root change evicts these
    /// by the same path it evicts everything else derived from a scan. If that
    /// ever stops being true, these two fields have to leave with it.
    let quotaCards: QuotaCards

    /// Every quota surface a rebuilt model would otherwise recompute before it
    /// could draw anything.
    struct QuotaCards {
        /// "Past windows".
        let summaries: [QuotaWindowSummary]
        /// "When the allowance goes".
        let heatmaps: [String: QuotaHeatmap]
        let heatmapWindows: [QuotaHeatmapWindow]
        /// The window card at the top of the lens, and its sparkline.
        let windowCards: [String: WindowCardState]
        /// Which account each card in `windowCards` was built for (scan slot,
        /// "" = primary). Restored with it: a card is only valid for the
        /// account it was built for.
        let windowCardAccounts: [String: String]
        let windowCurves: [String: [QuotaSample]]
        /// "Window history" at the foot of the lens, and the API-value
        /// estimates its rows are annotated with.
        let history: [QuotaHistoryRow]
        let equivalences: [String: WindowEquivalence.Row]
        /// The cycles the history card draws. `QuotaHistoryCard` branches on
        /// `cycles.isEmpty` and iterates them; `history` only annotates them.
        /// Restoring the annotation without the list left the card on its
        /// placeholder with the rows sitting behind it unused — the assertion
        /// that missed it compared `quotaHistory.count`, which is a model
        /// property and not what the card draws from.
        let cycles: [QuotaCycle]
        /// Which window `history` was built for. Restored with it so the first
        /// rebuild on a reopened model can tell "the same window, scan not back
        /// yet" from "a different window", and retain only the former.
        let historyCardId: String?
        /// The scan slot of the account `cycles` belong to, so a reopened
        /// model whose resolved account has changed since does not draw them
        /// (spec rule 3). Nil only for a snapshot that recorded none.
        var cyclesAccount: String? = nil
    }
}

/// Shared dashboard data for every lens. Base data (graph + model report)
/// loads when the popover opens; the hourly/agents reports load lazily the
/// first time their lens becomes active, mirroring the Tauri app's
/// empty-year short-circuit hooks.
@MainActor @Observable final class DashboardModel {
    private struct HourlyCacheKey: Hashable {
        let year: String
        let clients: Set<String>
    }

    /// The (year, graph generation) pair a model report belongs to. Comparing
    /// the whole pair is what lets an in-flight scan be reused for a re-entry
    /// on the same slice while still being superseded by a genuinely newer one.
    private struct ModelSliceIdentity: Equatable {
        let year: String
        let generation: String
    }

    /// Survives the model's deallocation so the next PopoverView starts with
    /// cached data instead of `.loading`. A deliberate process-lifetime cache
    /// (one COW-shared value snapshot, never invalidated). Every model may
    /// *read* it on init, but only the most recently initialized popover model
    /// may write it: an older instance can outlive its view while an FFI scan
    /// finishes, and must not roll back the next popover's fresher snapshot.
    private static var lastSnapshot: DashboardSnapshot?

    /// Test seam (not `#if DEBUG`: the bundled selftest uses it): what the
    /// reopen cache holds.
    static var lastSnapshotForTesting: (payload: UsagePayload, knownYears: [String], modelReport: ModelReport?)? {
        lastSnapshot.map { ($0.payload, $0.knownYears, $0.modelReport) }
    }

    /// Test seam: the process-wide hourly cache's reports.
    static var hourlyCacheForTesting: [HourlyReport] { Array(hourlyCache.values) }

    /// The union scan, held across popover reopens like `lastSnapshot` is.
    /// `@State` on PopoverView does not survive the view being rebuilt, so an
    /// instance property here would make every reopen pay the full scan again
    /// — which is exactly what the staging was meant to stop being visible.
    ///
    /// In memory only, never on disk: it holds local usage rows, and the disk
    /// envelope structurally refuses that class of data.
    /// One scan per account, keyed by `accountKey` — nil for the primary,
    /// spelled `""` here because a dictionary cannot key on nil.
    ///
    /// Per account rather than one shared scan: a quota window belongs to one
    /// account, and a scan that spans accounts folds another account's work
    /// into the allowance this one reports (issue #258). The engine narrows
    /// each scan to its account's roots, so selecting the right entry here is
    /// the whole of the Swift side's job.
    private static var lastUnionScans: [String: UnionScan] = [:]
    private static var lastSnapshotOwner: ObjectIdentifier?
    /// Reopen cache for the expensive hourly fold. Multiple slices coexist so
    /// Daily/Monthly's Codex+Claude report cannot evict Hourly's all-client one.
    // ponytail: FIFO at eight slices bounds memory; use LRU only if churn shows misses.
    private static let hourlyCacheLimit = 8
    private static var hourlyCache: [HourlyCacheKey: HourlyReport] = [:]
    private static var hourlyCacheOrder: [HourlyCacheKey] = []

    /// Drop every process-wide cache holding scan-derived data.
    ///
    /// Called when the extra-scan-root registry is replaced. The engine clears
    /// its own caches there and the setter's contract is that the next report
    /// picks up the new roots — but these live on THIS side of the FFI, and
    /// each serves its answer without asking the engine: the union scan for
    /// `unionScanMaxAge`, the hourly fold and the reopen snapshot until
    /// something evicts them. A root added in Settings would otherwise be
    /// missing from the quota cards, or a removed one still counted, for as
    /// long as the Swift copy outlived the change the engine had already
    /// applied.
    @MainActor
    static func invalidateScanDerivedCaches() {
        // Supersede in-flight loads, not just completed values. A suspended
        // `refreshWindowUsage` snapshotted its range before the roots changed
        // and the FFI returns its pre-change result regardless, so clearing
        // alone leaves the old task free to repopulate what was just cleared.
        // Advancing the token makes its own `windowScanToken == scanToken`
        // guard drop it — the guard already existed, it was simply never told
        // that a root change also overtakes a scan.
        windowScanToken &+= 1
        lastUnionScans.removeAll()
        lastSnapshot = nil
        lastSnapshotOwner = nil
        // The PERSISTED snapshot needs no clearing here, and deliberately gets
        // none. It records the root set it was built under, so the restore in
        // `init` rejects it by comparison — which also covers the routes a
        // clear cannot reach: the process dying between the edit and the
        // clear, and the registry being changed while the app is not running.
        // Clearing it here as well would only turn a rejected restore into a
        // missing one.
        hourlyCache = [:]
        hourlyCacheOrder = []
        // The fourth one, and it does not live on this type. The first version
        // of this function cleared the three statics it could see from here and
        // missed the attributed series' own reopen cache, which republishes
        // before awaiting a fresh graph — so the subscription trend kept
        // showing a removed root. "Every process-wide cache holding
        // scan-derived data" is the rule; being declared elsewhere is not an
        // exemption from it.
        AttributedSeriesModel.invalidateRowCache()
    }
    /// Whether this model participates in the shared `lastSnapshot` cache.
    /// The newest participating instance becomes its sole writer.
    private let cachesSnapshot: Bool
    private let source: any UsageDataSource
    /// The exact build this process ships as, or nil for anything that is not
    /// the shipping bundle (including every `swift run` invocation — demo,
    /// smoke, selftest, icon-gallery among them). A disk snapshot is read or
    /// written only when this is non-nil.
    private let buildIdentity: BuildIdentity?
    /// Resolved from the injected `snapshotDirectory` autoclosure ONLY when
    /// `cachesSnapshot && buildIdentity != nil` — never merely evaluated by
    /// default. `SnapshotStore.defaultDirectory()` is itself real
    /// `FileManager` work, and the isolation contract this app relies on
    /// (never touch the production location outside the shipping bundle) is
    /// that it is never even RESOLVED off that identity, not only never
    /// written to.
    private let resolvedSnapshotDirectory: URL?
    enum Phase {
        case loading
        case ready
        case failed(String)
    }

    private(set) var phase: Phase
    private static let yearKey = "tokenbar.dashboard.year"

    /// Empty is the explicit identity for an all-time slice; nil below means
    /// that no accepted payload/report exists yet, so it cannot masquerade as
    /// all-time.
    private static func identityYear(_ year: String?) -> String { year ?? "" }

    /// Resolve the active year filter: the `--year=` debug flag wins, else the
    /// persisted selection. Used as `init()`'s default so the snapshot guard and
    /// the model's `year` can never drift (the guard MUST compare the same value
    /// the model fetches, or it would mis-classify a consistent snapshot as stale).
    /// Callers that own process-wide settings may explicitly pass nil for all time.
    private static func resolveYear() -> String? {
        CommandLine.arguments
            .first(where: { $0.hasPrefix("--year=") })
            .map { String($0.dropFirst("--year=".count)) }
            ?? UserDefaults.standard.string(forKey: yearKey)
    }

    private static let snapshotDecoder = JSONDecoder()

    /// `cachesSnapshot` = true only for the popover's model (PopoverView), the
    /// one whose per-open teardown/rebuild the cache exists to speed up; the
    /// settings window passes false so it never writes the shared snapshot,
    /// and never reads the disk one either.
    ///
    /// `buildIdentity`/`snapshotDirectory` are injectable so a hermetic test
    /// can drive the disk path without ever touching the production location
    /// — production always takes the defaults. `snapshotDirectory` is an
    /// `@autoclosure` specifically so it is not evaluated unless
    /// `cachesSnapshot && buildIdentity != nil`; see `resolvedSnapshotDirectory`.
    init(
        cachesSnapshot: Bool = false,
        source: any UsageDataSource = UsageDataSources.current,
        initialYear: String? = DashboardModel.resolveYear(),
        buildIdentity: BuildIdentity? = BuildIdentity.shipping(),
        snapshotDirectory: @autoclosure () -> URL? = SnapshotStore.defaultDirectory(),
        deviceDefaults: UserDefaults = .standard
    ) {
        self.cachesSnapshot = cachesSnapshot
        self.source = source
        self.deviceOverlay = DeviceOverlayState(source: source, defaults: deviceDefaults)
        self.year = initialYear
        self.buildIdentity = buildIdentity
        self.resolvedSnapshotDirectory =
            (cachesSnapshot && buildIdentity != nil) ? snapshotDirectory() : nil

        // Guard snapshot restore on year-consistency: if the user changed the
        // year filter after the snapshot was written (e.g. setYear() persisted
        // the new year but reload() failed before apply() ran), the cached
        // payload is for the wrong slice — fall through to .loading so load()
        // fetches fresh. Settings passes nil explicitly because its client-item
        // controls must use the same all-time graph universe as AppDelegate.
        if let snap = Self.lastSnapshot, snap.year == initialYear {
            payload = snap.payload
            stats = snap.stats
            modelReport = snap.modelReport
            colors = snap.colors
            knownYears = snap.knownYears
            // Draw the history cards from the last publication rather than
            // from nothing, then let the refresh below replace them in place.
            // `refreshWindowQuotaHalves()` runs synchronously on every open
            // (`PopoverView`), so what is restored here is visible only until
            // that pass lands — the same trade `payload` and `stats` above
            // already make, over data that is append-only and barely moves
            // between two opens.
            quotaWindowSummaries = snap.quotaCards.summaries
            quotaHeatmaps = snap.quotaCards.heatmaps
            quotaHeatmapWindows = snap.quotaCards.heatmapWindows
            windowCards = snap.quotaCards.windowCards
            windowCardAccounts = snap.quotaCards.windowCardAccounts
            windowCurves = snap.quotaCards.windowCurves
            quotaHistory = snap.quotaCards.history
            quotaEquivalences = snap.quotaCards.equivalences
            quotaCycles = snap.quotaCards.cycles
            quotaCyclesAccount = snap.quotaCards.cyclesAccount
            quotaHistoryCardId = snap.quotaCards.historyCardId
            // Seeded from the SAME id, because two separate checks compare
            // against it and both read a nil as "a different window". A
            // restored model whose first curve read throws takes the failure
            // branch in `refreshWindowQuotaHalves`, which clears `quotaHistory`
            // when `quotaCyclesCardId != selected` — nil always differs, so the
            // rows this restore just put on screen were dropped by the very
            // case the retention was added for. `rebuildQuotaHistory`'s own
            // check has the same shape.
            quotaCyclesCardId = snap.quotaCards.historyCardId
            // A restored non-empty strip HAS been published in this process,
            // by the model that cached it. Without this the #356 guard reads
            // the restored rows as "never published" and an unanswered refresh
            // is free to replace them with an empty set — which is the state
            // that guard exists to prevent, reintroduced through the restore.
            publishedWindowSummaries = !snap.quotaCards.summaries.isEmpty
            acceptedPayloadYear = Self.identityYear(initialYear)
            // Restore the model's own slice identity alongside it, or the first
            // model-dependent lens would re-request a report the snapshot
            // already carries and flash a loading state on every reopen. Use
            // the model's recorded generation rather than the payload's: when
            // it lags, the lens SHOULD re-request instead of treating a stale
            // report as current.
            var modelCurrent = false
            if snap.modelReport != nil {
                modelYear = Self.identityYear(initialYear)
                modelPayloadGeneratedAt = snap.modelGeneratedAt
                modelFetchedAt = snap.modelFetchedAt
                modelCurrent = snap.modelGeneratedAt == snap.payload.meta.generatedAt
                    && Self.isFresh(
                        modelFetchedAt: snap.modelFetchedAt,
                        headroom: Self.modelReportRestoreHeadroom)
            }
            agentUsage = snap.agentUsage.map {
                AgentUsagePublicationCoordinator.resolve($0)
            }
            trace = snap.trace
            phase = .ready
            restoredSnapshot = RestoredSnapshot(savedAt: snap.payloadCapturedAt, failed: false)
            // Installed ONLY when the restored model report is absent or lags
            // its payload — a restore whose model is already current for the
            // committed generation never has anything for the gate to guard
            // (`ensureModelReport` returns at its own identity check).
            if !modelCurrent { restoreGatePending = true }
        } else if cachesSnapshot, let identity = buildIdentity,
                  let directory = resolvedSnapshotDirectory,
                  let bytes = SnapshotStore.readBytes(in: directory),
                  let envelope = try? Self.snapshotDecoder.decode(SnapshotEnvelope.self, from: bytes),
                  SnapshotStore.validate(
                      envelope, expectedYear: initialYear, identity: identity,
                      scanRoots: ClaudeExtraRoots.appliedPayloadJSON)
        {
            // The model report is never persisted (see SnapshotEnvelope's doc
            // comment), so a disk restore ALWAYS leaves modelReport/modelYear/
            // modelPayloadGeneratedAt nil and the gate is always installed.
            payload = envelope.payload
            stats = UsageStats(
                payload: envelope.payload, selectedClients: Set(envelope.payload.summary.clients))
            knownYears = envelope.knownYears
            acceptedPayloadYear = Self.identityYear(initialYear)
            phase = .ready
            restoredSnapshot = RestoredSnapshot(savedAt: envelope.savedAt, failed: false)
            restoreGatePending = true
        } else {
            phase = .loading
        }
        if cachesSnapshot {
            // A restore with no quota (disk branch, or an in-memory snapshot
            // dropped by `invalidateScanDerivedCaches`) would open the quota
            // area empty until a full provider round. The tray keeps the
            // process-wide publication fresh, so show that while the poll
            // refreshes it. Only when nothing was restored, and only under the
            // account set the payload was accepted with.
            if agentUsage == nil {
                agentUsage = AgentUsagePublicationCoordinator.latestPayload(
                    forAccountSet: AgentUsagePublicationCoordinator.accountSet)
            }
            Self.lastSnapshotOwner = ObjectIdentifier(self)
        }
    }

    /// Year filter for every lens (HeaderBar's year select in the Tauri app);
    /// nil = all time. Persisted so the selection survives the popover's
    /// rootView teardown/rebuild cycle.
    /// `--year=<yyyy>` preselects a year (debug/screenshot aid).
    private(set) var year: String?
    /// Union of `payload.years` across loads — a year-filtered payload only
    /// reports the selected year, so remember the rest for the picker.
    private(set) var knownYears: [String] = []

    /// Clients that have had local usage records in ANY payload this install
    /// has loaded, persisted across launches. `stats.presentClients` covers
    /// only the selected year, while a window card's local scan covers its
    /// quota history whatever the year: gating the scan on the year's clients
    /// turned a Codex card into "can't be attributed" on a year with no Codex
    /// records (#462 review, Windows lane F). Client ids only; it only grows,
    /// which errs toward scanning (a scan of a client with no records shows
    /// zero rather than hiding real usage).
    static let localRecordClientsKey = "tokenbar.localRecordClients"

    static func recordLocalRecordClients(_ ids: [String], defaults: UserDefaults = .standard) {
        let known = Set(defaults.stringArray(forKey: localRecordClientsKey) ?? [])
        let union = known.union(ids)
        if union != known { defaults.set(union.sorted(), forKey: localRecordClientsKey) }
    }

    /// The year-independent "has local records" set the window-card gate reads:
    /// the persisted union plus whatever the current payload shows.
    func localRecordClients(defaults: UserDefaults = .standard) -> [String] {
        Array(Set(defaults.stringArray(forKey: Self.localRecordClientsKey) ?? [])
            .union(stats?.presentClients ?? [])).sorted()
    }
    /// SA-3d: the pages of the selected devices, merged by the core. Never
    /// read into any property below: see `DeviceOverlayState`.
    let deviceOverlay: DeviceOverlayState
    private(set) var payload: UsagePayload?
    private(set) var stats: UsageStats?
    private(set) var modelReport: ModelReport?
    private(set) var colors = ModelColorMap(report: nil)
    /// The graph fetch currently running, for the header's freshness
    /// indicator. Distinct from `graphLoadTask`: this is presentation state
    /// (token + trigger kind) rather than the task itself, and is cleared by
    /// ownership the same way `commit`/`graphFetchFailed` are — see
    /// `gatedGraph`.
    private(set) var backgroundRefresh: BackgroundRefresh?
    /// Set whenever `init` restored a payload (memory or disk) that has not
    /// yet been confirmed current by a live fetch. Cleared only by the first
    /// accepted commit — see `RestoredSnapshot`'s doc comment.
    private(set) var restoredSnapshot: RestoredSnapshot?
    private(set) var hourly: HourlyReport?
    /// True while a model-report request is in flight. Model-dependent cards
    /// must distinguish this from a completed request that genuinely found
    /// nothing, or a deferred model reads as "no usage" during startup.
    private(set) var modelLoading = false
    private(set) var agents: AgentsReport?
    private(set) var agentUsage: AgentUsagePayload?
    /// True once the first `pollAgentUsage()` attempt has finished, whether it
    /// succeeded or not. Lets a view show a terminal state instead of waiting on
    /// a payload that may never arrive.
    private(set) var agentUsageAttempted = false
    private(set) var trace: [TraceBucket] = []

    // Memo for the hidden-client Overview slice: lensContent re-evals on every
    // ~10s trace poll, and re-aggregating UsageStats (incl. Streaks' full-range
    // double pass) each time is wasteful. Keyed on the payload's generatedAt
    // plus the selected set, so it recomputes only when either changes.
    // @ObservationIgnored: pure derived cache, never a view dependency, so
    // reading/writing it during a view update triggers no observation churn.
    @ObservationIgnored private var statsMemoGeneratedAt: String?
    @ObservationIgnored private var statsMemoSelected: Set<String>?
    @ObservationIgnored private var statsMemoValue: UsageStats?

    // The client selection each lazy report was last fetched for. Hourly/agents
    // buckets fold all clients into mixed totals, so the slice is now applied at
    // the FFI (accurate per-client totals); these track it so a tab switch or a
    // hide toggle refetches the right slice instead of serving another tab's.
    // nil = never fetched. Set-valued so a reorder (same members) is not a
    // refetch. Background refreshes (reload/pollGraph) reuse the stored slice.
    @ObservationIgnored private var hourlyClients: Set<String>?
    @ObservationIgnored private var agentsClients: Set<String>?
    /// Identity of the slice the current `modelReport` was fetched for. The
    /// year keeps a previous year's report off a newly-filtered dashboard; the
    /// payload generation is what makes the request idempotent per graph commit,
    /// so a model-dependent lens re-requests only when the graph actually moves.
    /// Readable because the attribution breakdown states the range its figures
    /// cover, and that has to be the range the report was fetched for rather
    /// than the currently selected year — a failed reload can move the selection
    /// while this report stands, and labelling stale rows with the new year is
    /// exactly the misreading the feature exists to prevent.
    @ObservationIgnored private(set) var modelYear: String?
    @ObservationIgnored private var modelPayloadGeneratedAt: String?
    /// When the displayed report was fetched. The payload generation alone
    /// cannot key the report: `tb_graph` keeps returning the same generation
    /// while logs are idle, but the report also carries prices, which refresh
    /// independently of the logs (#191). So a report current for its
    /// generation still expires after `modelReportMaxAge`, and the 60-second
    /// `retryModelIfStale` in `pollGraph` re-fetches it.
    ///
    /// The cost is one warm `tb_model_report` scan per hour while the poll
    /// runs: what #187 removed from every poll, bounded to the pricing cache's
    /// TTL. An FFI entry reporting the pricing timestamp alone would avoid the
    /// scan; #191 chose the hourly re-fetch over adding one.
    @ObservationIgnored private var modelFetchedAt: Date?
    /// Matches the engine's pricing cache TTL (`CACHE_TTL_SECS` and
    /// `REMOTE_PRICING_TTL_SECS`, both 3600 in the pinned tokscale-core): a
    /// re-fetch sooner could not observe new prices anyway.
    static let modelReportMaxAge: TimeInterval = 3600
    /// How much of `modelReportMaxAge` a restored report must still have left
    /// to count as current. The restore decides whether to install the LP3
    /// gate once, in `init`; a report that expired between then and the first
    /// `ensureModelReport` would be re-fetched with no gate, before `load()`
    /// confirms the payload. Five minutes is far wider than that gap, which is
    /// not measured.
    static let modelReportRestoreHeadroom: TimeInterval = 300
    /// A negative age means the wall clock moved back past the fetch; that
    /// counts as expired, so a clock change cannot freeze prices for longer.
    private static func isFresh(modelFetchedAt: Date?, headroom: TimeInterval = 0) -> Bool {
        guard let modelFetchedAt else { return false }
        let age = Date().timeIntervalSince(modelFetchedAt)
        return age >= 0 && age < modelReportMaxAge - headroom
    }
    /// Moves the fetch time back, so a case can cross `modelReportMaxAge`
    /// without waiting an hour. Re-caches the snapshot so a reopen restores
    /// the aged time too.
    func ageModelReportForTesting(by seconds: TimeInterval) {
        modelFetchedAt = modelFetchedAt?.addingTimeInterval(-seconds)
        cacheSnapshot()
    }
    @ObservationIgnored private var modelRequestToken = 0
    /// The slice a model scan is currently running for, used to coalesce
    /// re-entry. Nil when nothing is in flight.
    @ObservationIgnored private var modelInFlight: ModelSliceIdentity?
    /// The unstructured task carrying the in-flight scan. Held so a re-entrant
    /// caller can await the same work instead of starting its own, and so the
    /// scan can be cancelled when its slice stops being displayed.
    @ObservationIgnored private var modelTask: Task<ModelReport?, Never>?
    /// Whether a model-dependent lens has asked for the report in this slice.
    /// Only then is a missing report worth retrying on the poll — a session that
    /// never leaves Daily must not pay for a scan it does not render.
    @ObservationIgnored private var modelWanted = false
    /// The graph fetch currently running, held so a deferred model request
    /// waits for it instead of racing it. EVERY graph fetch installs it — the
    /// initial load, a manual refresh and the 60s poll alike.
    ///
    /// A restored snapshot makes the dashboard renderable before `load()` has
    /// fetched anything, so the model task's key is already a real value on the
    /// very first body evaluation and the task fires at once. The "no model
    /// scan until a payload commits" property therefore held only for a
    /// first-ever open; every reopen whose snapshot lacked a current model
    /// report put both full scans back on the same bounded pool.
    ///
    /// Refresh and poll need the same gate for a different reason: the model
    /// task is keyed on the LENS as well as the committed slice, so opening
    /// Overview (or expanding a row) mid-refresh raises a request even though
    /// the slice key has not moved and no payload has committed.
    @ObservationIgnored private var graphLoadTask: Task<GraphFetchOutcome, Error>?

    /// What a gated fetch DID, which is not what it returned. A superseded
    /// fetch holds a perfectly good payload and commits nothing, so a caller
    /// that reads its completion as a settled slice acts on state the newer
    /// fetch is about to replace — the poll would start its model and lazy
    /// reports beside a graph scan still running, back on the bounded pool.
    /// Returning the payload made those two cases indistinguishable; naming
    /// them is what removes the question.
    private enum GraphFetchOutcome {
        case committed(ApplyResult)
        case superseded
    }

    /// Installed by `init` only when it restored a payload whose model report
    /// is absent or not current for it. Awaited by `ensureModelReport` before
    /// it reads ANY committed state — without that, a model-task-first
    /// ordering finds the restored payload already "committed" (year matches,
    /// non-nil) and a nil `graphLoadTask` (because `load()` has not even
    /// called `gatedGraph` yet), and would scan against a payload nobody has
    /// confirmed live. Fulfilled from INSIDE the gated task in `gatedGraph`,
    /// on every exit — success, failure, and superseded alike — so task
    /// cancellation on the caller's side can never strand it, and so
    /// `reload()`/`pollGraph()` fulfil it too if either runs before `load()`.
    @ObservationIgnored private var restoreGatePending = false
    @ObservationIgnored private var restoreGateContinuations: [CheckedContinuation<Void, Never>] = []

    private func waitForRestoreGate() async {
        guard restoreGatePending else { return }
        await withCheckedContinuation { continuation in
            // Re-checked here (not just by the guard above) because both
            // hops are MainActor-only: this is what makes "no window where
            // fulfillment and the wait cross" structural rather than assumed.
            if restoreGatePending {
                restoreGateContinuations.append(continuation)
            } else {
                continuation.resume()
            }
        }
    }

    private func fulfillRestoreGate() {
        guard restoreGatePending else { return }
        restoreGatePending = false
        let waiting = restoreGateContinuations
        restoreGateContinuations = []
        waiting.forEach { $0.resume() }
    }

    /// Monotonic capture sequence for the disk writer, taken on the MAIN
    /// actor before handing off to the detached encode — see `submitDiskCapture`.
    private static var nextCaptureSequence = 0

    /// Runs a graph fetch under that gate and commits it INSIDE the gated task,
    /// so the gate opens on the commit rather than on the fetch.
    ///
    /// Ordering, not just exclusion, is what the model request needs. With only
    /// the fetch gated, the waiter and the fetch's owner resume from the same
    /// task in an unspecified order: the waiter could go first, read the
    /// pre-commit payload, and scan for a generation that `apply()` was about
    /// to supersede — two full scans, the contention this split exists to
    /// remove.
    ///
    /// In practice the owner registers on the task first and does commit first
    /// — the fetch-gated shape was mutated back in and no assertion moved. So
    /// this removes a reliance on an unspecified ordering, not a reproduced
    /// defect, and no test discriminates the two shapes. It also closes the
    /// matching window where the slot still holds a finished task because the
    /// owner has yet to resume.
    ///
    /// The lazy-lens re-fetches stay outside: the model does not depend on
    /// them, and holding the gate across them would make it wait for nothing.
    /// `commit` runs only on success, and runs on this actor — its callers
    /// keep their own year guard, but not `Task.isCancelled`, which inside an
    /// unstructured task no longer describes the caller. `apply()` re-checks
    /// the year itself, and a commit that lands after a popover close only
    /// leaves a fresher reopen snapshot behind.
    /// `commit` receives the payload AND the scan roots that were installed
    /// when this fetch was issued.
    ///
    /// The roots must travel with the request rather than be read at capture
    /// time. The FFI returns a scan that started before a registry replace even
    /// after the replace has landed, so stamping a snapshot with the roots
    /// current at the moment of capture labels pre-change data as post-change —
    /// and the persisted-snapshot check then accepts it on the next launch,
    /// which is exactly the acceptance the fingerprint was added to withhold.
    /// Reading it here, before the fetch, is the only point at which the value
    /// is known to describe the scan.
    private func gatedGraph(
        kind: RefreshKind,
        fetch: @escaping () async throws -> UsagePayload,
        commit: @escaping (UsagePayload) -> ApplyResult
    ) async throws -> GraphFetchOutcome {
        let requestRoots = ClaudeExtraRoots.appliedPayloadJSON
        graphFetchToken += 1
        let token = graphFetchToken
        // Ownership-cleared, exactly like `graphFetchFailed` below: an
        // overtaken fetch's completion must not clear a NEWER request's
        // indicator, so every clear site compares the token it was given
        // against the CURRENT `backgroundRefresh`, not against its own copy.
        backgroundRefresh = BackgroundRefresh(token: token, kind: kind)
        let task = Task { () throws -> GraphFetchOutcome in
            do {
                let payload = try await fetch()
                // Same ownership rule as the failure path below, and for the
                // same reason: an overtaken fetch must not touch displayed
                // state. Two same-year fetches can overlap — a manual Refresh
                // started while `load()` or a poll is still running — and the
                // year guards cannot separate them, so an older result landing
                // second would roll the dashboard and the reopen snapshot back
                // to its payload and clear the newer fetch's failure state.
                // (The rollback itself predates the split; guarding only the
                // error path was this file's own asymmetry.)
                guard self.graphFetchToken == token else {
                    self.clearBackgroundRefresh(owner: token)
                    self.fulfillRestoreGate()
                    return .superseded
                }
                // Assigned rather than passed through `commit`: the closure
                // is the caller's, and threading a value it never mentions
                // through three call sites bought nothing over setting it on
                // the line before. Both statements are on the main actor with
                // no suspension between them, so `apply` cannot observe a
                // different fetch's value.
                self.payloadScanRoots = requestRoots
                let result = commit(payload)
                self.clearBackgroundRefresh(owner: token)
                self.fulfillRestoreGate()
                return .committed(result)
            } catch {
                // Inside the task for the same reason `commit` is: a waiter
                // that resumes the instant the gate opens must not read this
                // before the failure is recorded. Only the newest fetch may
                // record it — an overtaken one describes a slice that is no
                // longer displayed, exactly as above.
                if self.graphFetchToken == token {
                    self.graphFetchFailed = true
                    self.restoredSnapshot?.failed = true
                }
                self.clearBackgroundRefresh(owner: token)
                self.fulfillRestoreGate()
                throw error
            }
        }
        graphLoadTask = task
        defer { if graphLoadTask == task { graphLoadTask = nil } }
        return try await task.value
    }

    private func clearBackgroundRefresh(owner token: Int) {
        guard backgroundRefresh?.token == token else { return }
        backgroundRefresh = nil
    }
    /// Identity of the last payload/hourly report that actually committed.
    /// These are separate from `year`: a newer request may complete in the
    /// inverse order, and the presentation accessors must stay fail-closed.
    @ObservationIgnored private var acceptedPayloadYear: String?
    @ObservationIgnored private var hourlyYear: String?
    @ObservationIgnored private var hourlyRequestToken = 0

    /// UsageStats for a client slice, with hidden clients already removed from
    /// `selected`. Returns the precomputed full `stats` when the slice covers
    /// every present client (the common no-hidden case — no recompute); other-
    /// wise returns a memoized instance, recomputing only when the payload or
    /// the selected set changes. Call site: PopoverView.lensContent.
    func stats(selecting selected: Set<String>) -> UsageStats? {
        guard let payload, let stats else { return nil }
        if selected == Set(stats.presentClients) { return stats }
        if statsMemoGeneratedAt == payload.meta.generatedAt,
           statsMemoSelected == selected, let memo = statsMemoValue {
            return memo
        }
        let computed = UsageStats(payload: payload, selectedClients: selected)
        statsMemoGeneratedAt = payload.meta.generatedAt
        statsMemoSelected = selected
        statsMemoValue = computed
        return computed
    }

    /// The Hourly lens can render a matching report as soon as it completes;
    /// unlike Daily/Monthly it intentionally does not wait for the graph
    /// payload, preserving the existing hourly behavior.
    func hourlyReport(for clients: [String]) -> HourlyReport? {
        guard let hourly,
              hourlyClients == Set(clients),
              hourlyYear == Self.identityYear(year)
        else { return nil }
        return hourly
    }


    /// Identity of the slice whose payload is actually committed and rendering.
    ///
    /// Distinct from `year`, which moves the instant the user picks a filter —
    /// before the payload catches up. Keying the model task on `year` meant the
    /// id changed at the moment of intent and then stayed put when the new
    /// payload landed, so a slice whose generation matched the previous one
    /// never re-fired the task and no model request followed. Two slices can
    /// share a generation: an all-years payload and a current-year payload are
    /// both dated today.
    var committedSliceKey: String {
        "\(acceptedPayloadYear ?? "-")|\(payload?.meta.generatedAt ?? "")"
    }

    /// The source owns the blocking FFI hop in live mode; demo mode returns
    /// synthetic values through the same async contract.
    ///
    /// Graph-first: the model report is NOT fetched here. Both are blocking FFI
    /// scans that share one bounded Rayon pool, so issuing them together made
    /// each pay for the other's contention — measured on a real corpus, graph
    /// alone returned in 1.4s warm / 27s cold, but concurrently with the model
    /// (and the lazy hourly) it took 7.2s / 67s to reach the same first paint
    /// without finishing any sooner overall. The dashboard only needs the graph
    /// to render, so model-dependent lenses request it afterwards through
    /// `ensureModelData(for:)`, keyed on the committed payload generation.
    func load() async {
        do {
            let year = self.year
            _ = try await gatedGraph(kind: .initial) { [source] in
                try await source.graph(year: year, priority: .userInitiated)
            } commit: { payload in
                // The year may have changed while we were off-actor (the user
                // can open the year menu during the initial load); drop a stale
                // slice so apply() never tags the new year — and the static
                // snapshot — with the old year's payload. Mirrors
                // reload()/pollGraph().
                guard self.year == year else { return .rejected }
                return self.apply(payload: payload, expectedYear: year)
            }
        } catch {
            // Keep showing stale data over an error screen when a previous
            // load succeeded — a transient failure must not blank the UI.
            if payload == nil {
                phase = .failed("Failed to load usage: %@".localized(String(describing: error)))
            }
        }
    }

    private(set) var refreshing = false

    /// Manual refresh: force a full log re-read (bypassing the staticlib's
    /// 30s cache) and drop the lazy per-lens reports so they re-fetch.
    func refresh() async {
        guard !refreshing else { return }
        refreshing = true
        defer { refreshing = false }
        deviceOverlay.manualRefresh()
        await reload(force: true, kind: .manual)
    }

    /// A scan-root change, which must supersede whatever is in flight.
    ///
    /// Deliberately NOT `refresh()`. That method returns at `guard !refreshing`
    /// when a manual refresh or an earlier root apply is still running — and
    /// then it never enters `gatedGraph`, never advances `graphFetchToken`, and
    /// the older scan's old-root payload commits after the invalidation. The
    /// case the guard exists for (a user pressing Refresh twice) is the case
    /// this must not obey: two quick Settings edits are exactly when the
    /// superseding matters.
    ///
    /// It also leaves `refreshing` alone rather than setting it, so it cannot
    /// clear a flag a concurrent manual refresh owns.
    func reloadForRootChange() async {
        await reload(force: true, kind: .manual)
    }

    /// Switch the year filter and re-fetch every lens for the new slice.
    /// Served from the staticlib's per-year cache when fresh, so flipping
    /// back to a recent year is instant.
    func setYear(_ newYear: String?) async {
        guard newYear != year, !refreshing else { return }
        year = newYear
        invalidateHourly()
        invalidateModel()
        UserDefaults.standard.set(newYear, forKey: Self.yearKey)
        refreshing = true
        defer { refreshing = false }
        await reload(force: false, kind: .yearSwitch)
    }

    /// Auto-clear a year filter scoped to a year that only hidden clients used.
    /// The best-effort year picker can't drop such a year while it is the active
    /// selection (the payload is year-scoped then), so a dashboard already
    /// stranded on it — or one where the user just hid the year's only client —
    /// would show an empty slice. When the CURRENT year-scoped payload has no
    /// visible (non-hidden) stripe, fall back to All years via `setYear(nil)`,
    /// which reuses the existing year-clear discipline (persist + reload with
    /// the stale-year guards). No-op on All-years, before data loads, or when
    /// any visible activity exists. Reactive: PopoverView calls this on a hide
    /// toggle and on payload load.
    func clearYearIfHiddenOnly(hidden: Set<String>) async {
        guard year != nil, let payload, !refreshing else { return }
        if !UsageStats.hasVisibleActivity(contributions: payload.contributions, hidden: hidden) {
            await setYear(nil)
        }
    }

    private func beginHourlyRequest() -> Int {
        hourlyRequestToken += 1
        return hourlyRequestToken
    }

    private func beginModelRequest() -> Int {
        modelRequestToken += 1
        return modelRequestToken
    }

    /// Drop a model report that belongs to a slice the dashboard no longer
    /// shows. Also cancels any in-flight request so its late completion cannot
    /// publish the previous year's models.
    private func invalidateModel() {
        modelReport = nil
        colors = ModelColorMap(report: nil)
        modelYear = nil
        modelPayloadGeneratedAt = nil
        modelFetchedAt = nil
        modelLoading = false
        // Release the coalescing slot too: the in-flight scan belongs to the
        // slice being discarded, and leaving its identity set would let it
        // block a legitimate request should the user return to that slice.
        modelInFlight = nil
        modelTask?.cancel()
        modelTask = nil
        modelWanted = false
        // Every caller discards the model because a new slice is arriving, so
        // the honest state is "not known yet", not "none". Setting it here
        // rather than relying on a task to enter and set it is what survives a
        // key that does not change until the payload commits.
        modelLoading = true
        _ = beginModelRequest()
    }

    private func publishModel(_ report: ModelReport, year: String?, generation: String) {
        modelReport = report
        colors = ModelColorMap(report: report)
        modelYear = Self.identityYear(year)
        modelPayloadGeneratedAt = generation
        modelFetchedAt = Date()
        cacheSnapshot()
    }

    private func invalidateHourly() {
        hourly = nil
        hourlyClients = nil
        hourlyYear = nil
        _ = beginHourlyRequest()
    }

    private func publishHourly(
        _ report: HourlyReport, year: String?, clients: Set<String>
    ) {
        let yearKey = Self.identityYear(year)
        hourly = report
        hourlyClients = clients
        hourlyYear = yearKey
        if cachesSnapshot {
            let cacheKey = HourlyCacheKey(year: yearKey, clients: clients)
            if Self.hourlyCache[cacheKey] == nil {
                Self.hourlyCacheOrder.append(cacheKey)
                if Self.hourlyCacheOrder.count > Self.hourlyCacheLimit {
                    Self.hourlyCache.removeValue(forKey: Self.hourlyCacheOrder.removeFirst())
                }
            }
            Self.hourlyCache[cacheKey] = report
        }
    }

    private func reload(force: Bool, kind: RefreshKind) async {
        let year = self.year
        // Graph-first here too: the model is refreshed after the graph commits,
        // and only when a lens had already loaded it (mirrors hourly/agents).
        let outcome: GraphFetchOutcome
        do {
            outcome = try await gatedGraph(kind: kind) { [source] in
                force
                    ? try await source.refreshGraph(year: year, priority: .userInitiated)
                    : try await source.graph(year: year, priority: .userInitiated)
            } commit: { payload in
                guard self.year == year else { return .rejected }
                return self.apply(payload: payload, expectedYear: year)
            }
        } catch {
            // A model that has never reached `.ready` must still settle. apply()
            // spawns this reload for an emptied year filter and returns BEFORE
            // setting `.ready`, so a failure here would otherwise strand phase on
            // `.loading` forever — the dashboard spins and Settings keeps
            // "looking for clients". Once ready, keep the stale-data-over-error
            // behavior a manual refresh relies on.
            if case .loading = phase {
                phase = .failed("Failed to load usage: %@".localized(String(describing: error)))
            }
            return
        }
        // A superseded fetch settled nothing: the newer one owns this slice and
        // re-fetches these lenses itself. Driving them here would put an hourly
        // and an agents scan beside a graph scan still running.
        guard case .committed = outcome else { return }
        // If apply() cleared a now-empty year filter, it spawned its own
        // unfiltered reload that re-fetches the lazy lenses for the new (nil)
        // year — skip the stale-`year` re-fetch here, or an empty year-filtered
        // hourly/agents could land after it and blank those lenses.
        guard self.year == year, !Task.isCancelled else { return }
        // Heal a stale model report that a lens already asked for, just as the
        // poll does. The shared seam owns the wanted/stale checks and coalesces
        // PopoverView's generation-keyed refire onto the same in-flight scan.
        await retryModelIfStale(priority: .userInitiated)
        // The retry may suspend while cancellation or a year change retires this
        // reload; do not issue lazy work for the slice it no longer owns.
        guard self.year == year, !Task.isCancelled else { return }
        // Re-fetch the lazy lenses that were already loaded, keeping the slice
        // they were last fetched for (an ordered array of the stored Set — the
        // FFI filter is membership-based, so order is irrelevant). Re-check the
        // stored slice AFTER the await: a tab switch during the fetch commits a
        // new slice via ensureData, and the slice-keyed `.task` won't refetch
        // (its key already records the new tab), so a stale overwrite here would
        // strand the wrong slice on the lens.
        if hourly != nil {
            let captured = hourlyClients
            let requestToken = beginHourlyRequest()
            let report = try? await source.hourlyReport(
                year: year, clients: captured.map(Array.init), priority: .userInitiated)
            if !Task.isCancelled,
               self.year == year,
               self.hourlyClients == captured,
               self.hourlyYear == Self.identityYear(year),
               hourlyRequestToken == requestToken,
               let report,
               let captured
            {
                publishHourly(report, year: year, clients: captured)
            }
        }
        if agents != nil {
            let captured = agentsClients
            let report = try? await source.agentsReport(
                year: year, clients: captured.map(Array.init), priority: .userInitiated)
            if self.year == year, self.agentsClients == captured { agents = report }
        }
    }

    /// Commit a graph payload and make the dashboard renderable. The model
    /// report is owned separately (`ensureModelData`/`publishModel`) so a graph
    /// commit never has to wait for it, and a still-loading model never blanks
    /// the cards that already have last-good data.
    ///
    /// Returns what actually happened — see `ApplyResult`. Only `.applied`
    /// clears `restoredSnapshot`: a redirect or a rejection settles nothing,
    /// so restored data stays flagged as not-yet-confirmed.
    @discardableResult
    private func apply(payload: UsagePayload, expectedYear: String? = nil) -> ApplyResult {
        guard expectedYear == nil || self.year == expectedYear else { return .rejected }
        // A year-filtered payload reports only the selected year (empty if that
        // year has no data). Validate the filter against THIS fresh payload —
        // not the knownYears union, which never drops a year once seen — so a
        // selected year whose logs were deleted/moved (even while the popover
        // stays open) clears instead of stranding the dashboard on an empty
        // slice. Re-fetch unfiltered so all data shows immediately.
        if let year, !payload.years.contains(where: { $0.year == year }) {
            invalidateHourly()
            invalidateModel()
            acceptedPayloadYear = nil
            self.year = nil
            UserDefaults.standard.removeObject(forKey: Self.yearKey)
            Task { [weak self] in await self?.reload(force: false, kind: .yearSwitch) }
            return .redirectedToAllYears
        }
        // A model report fetched for a different year describes a slice this
        // payload no longer shows, so drop it rather than render it beside the
        // new graph; the model-dependent lens re-requests for the new slice.
        if modelReport != nil, modelYear != Self.identityYear(year) {
            invalidateModel()
        }
        self.payload = payload
        acceptedPayloadYear = Self.identityYear(year)
        graphFetchFailed = false
        stats = UsageStats(payload: payload, selectedClients: Set(payload.summary.clients))
        knownYears = Set(knownYears + payload.years.map(\.year)).sorted(by: >)
        Self.recordLocalRecordClients(payload.summary.clients)
        phase = .ready
        payloadCapturedAt = Date()
        restoredSnapshot = nil
        cacheSnapshot()
        submitDiskCapture()
        return .applied
    }

    /// When `apply()` last committed a payload — the timestamp `cacheSnapshot()`
    /// stamps into `DashboardSnapshot.payloadCapturedAt`. Deliberately NOT
    /// touched by `publishModel()`/`refreshSnapshotLiveData()`, which republish
    /// the cache for reasons unrelated to the graph moving.
    @ObservationIgnored private var payloadCapturedAt = Date()

    /// The scan roots the CURRENT payload was fetched under, captured before
    /// its request went out rather than when the snapshot is written. Seeded
    /// with the roots at construction so a capture that somehow precedes any
    /// apply() records this process's actual set rather than an empty one.
    /// The scan roots the CURRENT payload was fetched under, captured before
    /// its request went out rather than when the snapshot is written.
    @ObservationIgnored private var payloadScanRoots = ClaudeExtraRoots.appliedPayloadJSON

    /// The roots stamped into the snapshot for the committed payload.
    var payloadScanRootsForTesting: String { payloadScanRoots }

    private var ownsLastSnapshot: Bool {
        cachesSnapshot && Self.lastSnapshotOwner == ObjectIdentifier(self)
    }

    private func replaceLastSnapshot(_ snapshot: DashboardSnapshot) {
        guard ownsLastSnapshot else { return }
        Self.lastSnapshot = snapshot
    }

    /// Capture the full restore cache from the current state. `apply()` first
    /// commits the validated payload/year pair; `publishModel()` may then add a
    /// report for that same slice. No-op unless this is the newest popover model
    /// and a base payload has loaded.
    private func cacheSnapshot() {
        guard cachesSnapshot, let payload, let stats else { return }
        replaceLastSnapshot(DashboardSnapshot(
            payload: payload, stats: stats, payloadCapturedAt: payloadCapturedAt,
            modelReport: modelReport,
            modelGeneratedAt: modelPayloadGeneratedAt,
            modelFetchedAt: modelFetchedAt,
            colors: colors, knownYears: knownYears, year: year,
            agentUsage: agentUsage, trace: trace,
            quotaCards: DashboardSnapshot.QuotaCards(
                summaries: quotaWindowSummaries, heatmaps: quotaHeatmaps,
                heatmapWindows: quotaHeatmapWindows,
                windowCards: windowCards, windowCardAccounts: windowCardAccounts,
                windowCurves: windowCurves,
                history: quotaHistory, equivalences: quotaEquivalences,
                cycles: quotaCycles, historyCardId: quotaHistoryCardId,
                cyclesAccount: quotaCyclesAccount)))
    }

    /// Submit the DISK capture. Deliberately separate from `cacheSnapshot()`
    /// (the in-memory reopen cache, which both `apply()` and `publishModel()`
    /// write): only a graph commit reaches this, so a model landing on its own
    /// never triggers a disk write, and Settings' model (`cachesSnapshot ==
    /// false`) never resolves a directory to write to at all.
    private func submitDiskCapture() {
        guard cachesSnapshot, let identity = buildIdentity,
              let directory = resolvedSnapshotDirectory,
              let envelope = snapshotEnvelope(identity: identity)
        else { return }
        Self.nextCaptureSequence += 1
        let sequence = Self.nextCaptureSequence
        Task.detached(priority: .utility) {
            await SnapshotWriter.shared.submit(sequence: sequence, envelope: envelope, directory: directory)
        }
    }

    /// The envelope `submitDiskCapture` would write, built in one place so a
    /// self-test asserts the value that actually reaches disk.
    ///
    /// Not merely a refactor: an assertion on `payloadScanRoots` passes while
    /// the stamp beside it reads the CURRENT roots instead, which is the very
    /// substitution this field exists to prevent. The property is about what is
    /// written, so the test has to see what is written.
    func snapshotEnvelope(identity: BuildIdentity) -> SnapshotEnvelope? {
        guard let payload else { return nil }
        return SnapshotEnvelope(
            snapshotSchemaVersion: SnapshotEnvelope.schemaVersion,
            bundleIdentifier: identity.bundleIdentifier,
            shortVersion: identity.shortVersion,
            buildNumber: identity.buildNumber,
            savedAt: Date(),
            selectedYear: year,
            payload: payload,
            knownYears: knownYears,
            scanRoots: payloadScanRoots)
    }

    /// Refresh only the live, year-independent fields (agentUsage/trace) of the
    /// existing snapshot from their pollers, keeping the payload/year pair that
    /// apply() last wrote. The pollers run outside apply() and must NOT
    /// re-capture payload/year: self.year can momentarily disagree with
    /// self.payload mid year-switch (setYear flips year before reload's apply
    /// lands) or after the empty-year auto-clear, and writing that pair would
    /// mis-tag a stale payload with a changed year that the init guard can't
    /// catch. Preserving snap.payload/snap.year keeps the cache consistent.
    /// No-op until apply() has written a base snapshot.
    private func refreshSnapshotLiveData() {
        guard cachesSnapshot, let snap = Self.lastSnapshot else { return }
        replaceLastSnapshot(DashboardSnapshot(
            payload: snap.payload, stats: snap.stats, payloadCapturedAt: snap.payloadCapturedAt,
            modelReport: snap.modelReport,
            modelGeneratedAt: snap.modelGeneratedAt,
            modelFetchedAt: snap.modelFetchedAt,
            colors: snap.colors, knownYears: snap.knownYears, year: snap.year,
            agentUsage: agentUsage, trace: trace,
            // From the model, not from `snap`: this path republishes the cache
            // with live data, and the strip is live data. Copying `snap`'s
            // would pin the cache to whatever the strip held when the graph
            // was committed and undo every refresh since.
            quotaCards: DashboardSnapshot.QuotaCards(
                summaries: quotaWindowSummaries, heatmaps: quotaHeatmaps,
                heatmapWindows: quotaHeatmapWindows,
                windowCards: windowCards, windowCardAccounts: windowCardAccounts,
                windowCurves: windowCurves,
                history: quotaHistory, equivalences: quotaEquivalences,
                cycles: quotaCycles, historyCardId: quotaHistoryCardId,
                cyclesAccount: quotaCyclesAccount)))
    }

    /// Periodically re-derive every loaded lens so the popover advances while
    /// it stays open. StatusItemController tears down and rebuilds PopoverView
    /// on each open/close cycle, so `.task { load() }` runs on every open and
    /// this loop is cancelled on close — but while open, without this loop the
    /// overview bars never pick up today's usage until a manual Refresh. Uses
    /// the non-forced graph() path: the staticlib's mtime-aware cache makes
    /// idle ticks cheap and only re-aggregates when logs actually change.
    /// Keeps stale data on error (only assigns on success).
    func pollGraph() async {
        while !Task.isCancelled {
            // Sleep first: load()'s initial fetch already covers t=0.
            try? await Task.sleep(for: .seconds(60))
            if Task.isCancelled { break }
            // SA-3d: which devices can be chosen follows the local poll. Reads
            // the peer list only; nothing of it reaches the local data below.
            deviceOverlay.refreshPeers()
            // Don't race an in-flight manual Refresh or year switch.
            guard !refreshing else { continue }
            let year = self.year
            let fetched = try? await gatedGraph(kind: .poll) { [source] in
                try await source.graph(year: year, priority: .utility)
            } commit: { payload in
                // The year may have changed while we were off-actor; drop a
                // stale slice so the chart never flickers to the wrong year.
                guard self.year == year else { return .rejected }
                return self.apply(payload: payload, expectedYear: year)
            }
            if Task.isCancelled { break }
            // Same rule as reload: a superseded poll settled nothing, so its
            // model retry and lazy re-fetches would run beside the fetch that
            // overtook it.
            guard self.year == year, case .committed = fetched else { continue }
            // apply() may have cleared a now-empty year filter and spawned an
            // unfiltered reload; skip the stale-`year` lazy re-fetch so it
            // can't blank Hourly/Agents with empty year-filtered reports.
            guard self.year == year, !Task.isCancelled else { continue }
            // Retry a model report a lens asked for and does not have for the
            // committed slice. Before the graph/model split this poll re-fetched
            // it unconditionally, so a transient failure self-healed within 60s;
            // deferring the fetch removed that path. See `retryModelIfStale`
            // for why its condition is `modelWanted` and nothing else.
            await retryModelIfStale(priority: .utility)
            guard self.year == year, !Task.isCancelled else { continue }
            // Re-fetch the lazy lenses that were already loaded (mirrors reload),
            // keeping each one's last-fetched client slice.
            // Re-check the stored slice after the await (see reload()): a tab
            // switch mid-fetch must not let this background refresh overwrite
            // the fresh slice with the stale one.
            if hourly != nil {
                let captured = hourlyClients
                let requestToken = beginHourlyRequest()
                let report = try? await source.hourlyReport(
                    year: year, clients: captured.map(Array.init), priority: .utility)
                if !Task.isCancelled,
                   self.year == year,
                   self.hourlyClients == captured,
                   self.hourlyYear == Self.identityYear(year),
                   hourlyRequestToken == requestToken,
                   let report,
                   let captured
                {
                    publishHourly(report, year: year, clients: captured)
                }
            }
            if agents != nil {
                let captured = agentsClients
                let report = try? await source.agentsReport(
                    year: year, clients: captured.map(Array.init), priority: .utility)
                if self.year == year, self.agentsClients == captured { agents = report }
            }
        }
    }

    /// Card state per client. Never absent while an agent tab is open — the
    /// loading case is a state, not a nil.
    var windowCards: [String: WindowCardState] = [:]

    /// The account each `windowCards` entry was built for, as a scan slot
    /// ("" = primary). Written wherever `windowCards` is.
    var windowCardAccounts: [String: String] = [:]

    /// The card the views draw for `clientId`: the stored state only while it
    /// was built for the account now resolved. A held card for account A must
    /// never flash under account B's pills, so a mismatch is `.loading`.
    func windowCard(for clientId: String) -> WindowCardState? {
        guard let held = windowCards[clientId] else { return nil }
        let slot = Self.scanSlot(cardAccountKey(for: clientId))
        return windowCardAccounts[clientId] == slot ? held : .loading
    }

    /// The scan for one account, restored from the shared cache so a reopen
    /// renders bars immediately instead of spinning.
    ///
    /// `accountKey` is the card's own — `nil` is the primary, which is what
    /// every client but an extra Claude account has. There is deliberately no
    /// accessor that returns "the scan", because there is no longer one scan
    /// and a caller reaching for it would silently get the primary's.
    private func unionScan(for accountKey: String?) -> UnionScan? {
        Self.lastUnionScans[Self.scanSlot(accountKey)]
    }

    private func setUnionScan(_ scan: UnionScan?, for accountKey: String?) {
        Self.lastUnionScans[Self.scanSlot(accountKey)] = scan
    }

    /// Dictionary key for an account. The empty string is the primary, which no
    /// real config directory can collide with: `claude_config_dirs` refuses an
    /// empty path, and Settings refuses one too.
    static func scanSlot(_ accountKey: String?) -> String { accountKey ?? "" }

    /// The account a window CARD is about: `WindowCardAccount.resolve` over
    /// the published payload and the per-client stored choice. The one place
    /// that decides it; every read of "which account's card" goes through here.
    func cardAccountKey(for clientId: String) -> String? {
        WindowCardAccount.resolve(
            payload: agentUsage, clientId: clientId,
            stored: WindowCardAccount.stored(clientId: clientId))
    }

    /// The scan slot of the account `quotaCycles` / `quotaHistory` were last
    /// written for by `refreshWindowQuotaHalves` (both on success and when a
    /// failed read clears them). Nil until then (including after a reopen
    /// restore), meaning "not known to belong to another account".
    @ObservationIgnored private(set) var quotaCyclesAccount: String?

    /// The cycles and history rows for the open client's card, except while
    /// they belong to ANOTHER account: between an account pick and the refresh
    /// that rebuilds them, `quotaCycles` still holds the previous account's
    /// cycles, and drawing them under the new account is a cross-account
    /// history (spec rule 3). `pending` lets the history card show its loading
    /// state meanwhile. Compared by account only, not by card id: a failed
    /// read clears the id to nil, and the card must then keep showing
    /// "could not be read", not a spinner (verifier F1).
    func cardHistory(for clientId: String)
        -> (cycles: [QuotaCycle], rows: [QuotaHistoryRow], pending: Bool)
    {
        if let held = quotaCyclesAccount,
           held != Self.scanSlot(cardAccountKey(for: clientId)) {
            return ([], [], true)
        }
        return (quotaCycles, quotaHistory, false)
    }

    /// What the views need for the open client's account pills, header label
    /// and rule-6 line. Nil when the client has no live account at all.
    func cardAccountContext(for clientId: String) -> CardAccountContext? {
        let accounts = WindowCardAccount.accounts(payload: agentUsage, clientId: clientId)
        guard !accounts.isEmpty else { return nil }
        return CardAccountContext(
            clientId: clientId, accounts: accounts, resolved: cardAccountKey(for: clientId),
            // The scan client is set exactly when the tab has local records
            // (`WindowCardGate`): no scan means no local usage to attribute.
            tabHasLocalRecords: windowUsageClient == clientId,
            primaryEmail: agentUsage?.agents.first {
                $0.clientId == clientId && $0.accountKey == nil
            }?.identity?.email)
    }

    /// How long a scan is served before being refreshed. Matched to the
    /// engine's own oneshot age so the two layers do not disagree about what
    /// counts as fresh.
    private static let unionScanMaxAge: TimeInterval = 30

    /// Provider ids eligible for quota cards and history, including quota-only sources.
    var windowCardClients: [String] = []

    private var quotaVisibility: (tabs: Set<String>, limits: Set<String>, order: String)?

    /// The view supplies preferences, not a usage-derived provider list. Keep
    /// them for the next quota publication, which can arrive before graph data.
    func configureQuotaVisibility(tabHidden: Set<String>, limitsHidden: Set<String>, orderRaw: String) {
        quotaVisibility = (tabHidden, limitsHidden, orderRaw)
    }

    /// Quota readings for EVERY window each client offers, keyed
    /// `"<clientId>|<cardId>"` — the same identity the card's candidates use.
    ///
    /// `windowCards` above holds only the selected window, which is all the
    /// detail card needs. The Agent-limits sparkline draws one line per row, so
    /// it needs the others too. Each entry is one curve read per publication
    /// (`cachedQuotaCurve`), so filling all of them stays inside the "instant"
    /// half.
    var windowCurves: [String: [QuotaSample]] = [:]

    /// Curves already read under the payload generation now published, keyed
    /// per series. See `cachedQuotaCurve`.
    @ObservationIgnored private var curveCache: (generation: UInt64, curves: [String: QuotaCurve?]) = (0, [:])

    /// One curve read per series per publication, instead of one per use.
    ///
    /// Every read loads and parses the whole quota history store in the engine
    /// (`read_series` → `load_store_at_with_mode`), about 9.5ms each on the
    /// measuring machine in a release build. `refreshWindowQuotaHalves` used to
    /// read each window up to four times in one pass (the sparklines, the card,
    /// the strip, the cycles), 14 reads and 134ms of a 140ms pass, and it runs
    /// on the main actor on every window switch. Measured with
    /// `--refresh-timing`, not inferred.
    ///
    /// Keyed by generation because that is what the read is bound to: the
    /// engine refuses a read for any generation but the current publication
    /// (`quota_curve_result_with_reader`). This process records samples only
    /// inside a publication: every production writer found is a provider fetch
    /// in `agent_usage::run` (`apply_provider_outcome` → `enrich_snapshot`),
    /// which `tb_agent_usage` runs under `with_agent_usage_publication_gate`
    /// before binding the new generation. So its own writes always arrive with
    /// a new generation and drop the whole cache.
    /// A write by another process between two publications is not seen until
    /// the next one; that is at most one poll interval, and the card already
    /// shows that publication's payload.
    ///
    /// Only answers are kept, `nil` included, because `nil` is "no history" and
    /// is an answer. A throw is not cached: it means "could not be read", and
    /// the next refresh has to ask again rather than repeat the failure.
    private func cachedQuotaCurve(
        clientId: String, accountKey: String?, windowKey: String, generation: UInt64
    ) throws -> QuotaCurve? {
        if curveCache.generation != generation { curveCache = (generation, [:]) }
        let key = [clientId, accountKey.map { "a:" + $0 } ?? "-", windowKey].joined(separator: "\u{1F}")
        if let held = curveCache.curves[key] { return held }
        let curve = try source.quotaCurveSync(
            clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
        curveCache.curves[key] = .some(curve)
        return curve
    }

    /// Stage 1. Synchronous and local: reads the in-memory payload and the
    /// persisted quota curve. Refreshes from already published data without
    /// waiting for another network request or a local usage scan.
    func refreshWindowQuotaHalves() {
        if let visibility = quotaVisibility {
            windowCardClients = ClientRegistry.quotaClients(
                present: stats?.presentClients ?? [],
                quotaIds: agentUsage?.configuredClientIds ?? [],
                tabHidden: visibility.tabs, orderRaw: visibility.order)
        }
        let visibleAgents = (agentUsage?.agents ?? []).filter { agent in
            windowCardClients.contains(agent.clientId)
                // Limits switches hide the primary row. Extra Claude accounts
                // keep their own identity, as they do in AgentLimitsCard.
                && (agent.accountKey != nil
                    || !(quotaVisibility?.limits.contains(agent.clientId) ?? false))
        }
        if quotaVisibility != nil, agentUsage != nil {
            let keys = Set(visibleAgents.flatMap { agent in
                agent.uniqueCardWindows.map {
                    AccountIdentity(clientId: agent.clientId, accountKey: agent.accountKey)
                        .windowKey(cardId: $0.cardId)
                }
            })
            // A hidden/removed window must not survive through the transient
            // read retention below. In particular, no-allowance responses have
            // no windows, so their previous curve is no longer eligible.
            windowCurves = windowCurves.filter { keys.contains($0.key) }
            quotaWindowSummaries = quotaWindowSummaries.filter { keys.contains($0.id) }
            // Pruned with the summaries it describes. The recompute below only
            // runs once a client list is known; until then the summaries are
            // retained and pruned here, and so is this.
            quotaUnreadableClients.formIntersection(visibleAgents.map(\.clientId))
            quotaHeatmaps = quotaHeatmaps.filter { keys.contains($0.key) }
            quotaHeatmapWindows = quotaHeatmapWindows.filter { keys.contains($0.id) }
            qualifyingCycles = qualifyingCycles.filter { keys.contains($0.key) }
            quotaEquivalences = quotaEquivalences.filter { keys.contains($0.key) }
            windowCards = windowCards.filter {
                windowCardClients.contains($0.key)
                    && !(quotaVisibility?.limits.contains($0.key) ?? false)
            }
            windowCardAccounts = windowCardAccounts.filter { windowCards[$0.key] != nil }
        }
        let now = Int64(Date().timeIntervalSince1970 * 1000)
        // Deliberately NOT `try?`: swallowing the error here is what let a
        // transient generation expiry be reported to the user as "this window
        // has no recorded quota history".
        let readCurve: (String, String?, String, UInt64) throws -> QuotaCurve? = {
            [self] c, account, key, gen in
            try cachedQuotaCurve(clientId: c, accountKey: account, windowKey: key, generation: gen)
        }
        if let payload = agentUsage {
            for agent in visibleAgents {
                for window in agent.uniqueCardWindows {
                    // A failed read keeps whatever the row already had. Blanking
                    // it would drop a drawn sparkline to a bar for one refresh
                    // and put it back on the next — a flicker that says nothing.
                    // Read under the history account (a merged Antigravity
                    // primary reads its captured account's curve); the row is
                    // still stored under the snapshot's own accountKey.
                    guard let samples = WindowCardLoader.curveSamples(
                        payload: payload, clientId: agent.clientId,
                        accountKey: agent.historyReadAccountKey,
                        window: window, curve: readCurve, nowMs: now)
                    else { continue }
                    windowCurves[WindowCardLoader.curveKey(
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId)] = samples
                }
            }
        }
        for clientId in windowCardClients where !(quotaVisibility?.limits.contains(clientId) ?? false) {
            let account = cardAccountKey(for: clientId)
            let slot = Self.scanSlot(account)
            let state = WindowCardLoader.quotaHalf(
                payload: agentUsage, clientId: clientId, accountKey: account,
                attempted: agentUsageAttempted,
                curve: readCurve, nowMs: now)
            // Fold in a usage half we already hold, so a stage-1 refresh does
            // not throw away a completed scan and blink back to loading.
            //
            // A settled failure wins over any scan, at any age. This used to
            // test the scan's AGE instead — admitting a fresh scan even for a
            // client already marked failed — which held while the only way to
            // be marked failed was a throw from a scan of THIS range. It is no
            // longer: an unscannable range settles as failed without the
            // engine being asked, and `usageHalf` answers an unresolvable
            // window with an EMPTY usage half rather than `nil`, so this
            // branch published `.ready` with zero usage over a span nothing
            // had looked at — the exact claim the failure flag exists to stop.
            //
            // The age test is deleted rather than kept beside this one: it was
            // only ever reached with the flag set, so it stated this same rule
            // more weakly, and a redundant check that can disagree is a second
            // failure mode rather than a second line of defence.
            if case let .quotaOnly(q, _) = state,
               !windowScanFailed(for: clientId, accountKey: account),
               let scan = unionScan(for: account),
               let (settled, usage) = WindowCardLoader.usageHalf(
                   quota: q, scan: scan, confirmed: UsageAttribution.confirmed().records) {
                windowCards[clientId] = .ready(settled, usage)
                windowCardAccounts[clientId] = slot
            } else if case let .quotaOnly(q, _) = state,
                      windowScanFailed(for: clientId, accountKey: account) {
                // The loader cannot know this — it never runs the scan. Carried
                // in here so the card can say the scan failed instead of
                // spinning under a chart that is already drawn.
                windowCards[clientId] = .quotaOnly(q, scanFailed: true)
                windowCardAccounts[clientId] = slot
            } else if case .loading = state,
                      let held = windowCards[clientId]?.cardId,
                      windowCardAccounts[clientId] == slot,
                      held == WindowCardLoader.selectedCardId(
                          payload: agentUsage, clientId: clientId, accountKey: account) {
                // A curve read that throws is a transient generation expiry —
                // `quotaHalf` answers `.loading`, which is right as an answer
                // and wrong as a replacement. Overwriting sent a drawn card
                // back to "Waiting for quota…" until a later refresh, which is
                // the flicker the summaries block a few lines down already
                // refuses to produce. Keep what the row had; the next refresh
                // publishes the new state.
                //
                // Only when the held card is about the window now selected:
                // two selections share a client, so retaining by client alone
                // left the chart and totals on the window the user had just
                // navigated away from while the picker highlighted the new one.
                // And only for the same ACCOUNT, for the same reason one level
                // up: the held card is that account's window, not this one's.
                continue
            } else {
                // A card holding a scan that cannot answer for the window it
                // shows lands here, as `.quotaOnly(scanFailed: false)` —
                // "Reading local usage…" under a chart that is already drawn.
                // Deliberate, and bounded; both were measured, not assumed.
                //
                // Reachable when the selected window opens after the scan ended
                // while the union `from` still precedes `now`. `from` is the
                // MINIMUM start across every candidate window and the oldest
                // cycle, so the range guard in `refreshWindowUsage` passes, the
                // scan completes, and `usageHalf` then declines — `covers`
                // tests both ends.
                //
                // Bounded by two constants rather than by hope: the cached scan
                // stops being reused 30 seconds after capture
                // (`unionScanMaxAge`, tested at the cached branch) and
                // `pollAgentUsage` calls `refreshWindowUsage` at most 60 seconds
                // apart, so the next rescan carries `untilMs` past the window
                // and the card resolves itself inside about one poll interval.
                //
                // Deliberate because NOTHING FAILED. The scan succeeded; it was
                // taken before this window opened, which is the only true thing
                // to say about it. Settling as `scanFailed: true` would render
                // an error for a failure that did not occur and then silently
                // correct itself — a worse claim than "a scan is pending",
                // which is what is actually happening.
                windowCards[clientId] = state
                windowCardAccounts[clientId] = slot
            }
        }

        // Strip summaries read quota curves only, without scanning local usage.
        // Configured visibility prunes hidden/removed windows above even before
        // graph arrival. Callers supplying the client list directly retain their
        // loading state until discovery settles, then clear an empty selection.
        if windowCardClients.isEmpty, stats != nil {
            quotaWindowSummaries = []
            quotaUnreadableClients = []
            quotaHeatmaps = [:]
            quotaHeatmapWindows = []
            qualifyingCycles = [:]
        }
        if let payload = agentUsage, !windowCardClients.isEmpty {
            var collected: [(clientId: String, accountKey: String?, cardId: String, label: String,
                             cycles: [QuotaCycle])] = []
            var heatmaps: [String: QuotaHeatmap] = [:]
            var heatmapWindows: [QuotaHeatmapWindow] = []
            // A THROWN read is a transient generation expiry, which happens
            // when another publication lands while these synchronous reads run.
            // Skipping that window and publishing the rest would drop it from
            // the strip and the heatmap until some later refresh — the same
            // "absent because we could not ask" mistake, arriving through a
            // partial result rather than an empty one. A successful nil is
            // still genuinely no history and still skips.
            /// Which windows threw, keyed as `QuotaWindowSummary.id` and as the
            /// `quotaHeatmaps` dictionary is keyed — `WindowCardLoader.curveKey`
            /// is `AccountIdentity.windowKey`, so one set addresses both.
            var failedWindowIds: Set<String> = []
            // Windows this pass actually had a key to read. Separates "the
            // payload stopped offering an allowance", where clearing the strip
            // is correct, from "the payload still offers windows and not one of
            // them answered", where it is not.
            var windowsToRead = 0
            for agent in visibleAgents {
                for window in agent.uniqueCardWindows {
                    guard let key = window.paceStatus.historyKey,
                          let generation = payload.publicationGeneration
                    else { continue }
                    windowsToRead += 1
                    let attempt: QuotaCurve?
                    do { attempt = try readCurve(agent.clientId, agent.historyReadAccountKey, key, generation) }
                    catch {
                        // #359. WHICH window could not be read, not merely that
                        // one could not. A single flag made every provider share
                        // one provider's failure: `antigravity` answers
                        // "quota curve binding is unavailable" permanently — not
                        // the transient generation expiry this branch was
                        // written for — and that one throw suppressed the whole
                        // publication, so Claude's 132 recorded cycles never
                        // reached the strip and the card reported that nothing
                        // had ever been recorded. Measured with `--window-probe
                        // --generation-drift`, not inferred.
                        failedWindowIds.insert(WindowCardLoader.curveKey(
                            clientId: agent.clientId, accountKey: agent.accountKey,
                            cardId: window.cardId))
                        continue
                    }
                    guard let curve = attempt else { continue }
                    let points = curve.points
                    let grid = QuotaHeatmapFold.build(points: points)
                    // See `WindowCardLoader.curveKey`: two accounts of the
                    // same client can offer identically-carded windows, so the
                    // plain "clientId|cardId" key would let one overwrite the
                    // other's grid.
                    heatmaps[WindowCardLoader.curveKey(
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId)] = grid
                    // Unplaced-only windows stay in the picker. `total` is
                    // zero when every reading pair straddles more than six
                    // hours, but the allowance still moved; dropping the window
                    // made the card report "nothing recorded yet" and put the
                    // line that explains it out of reach.
                    if grid.hasMovement {
                        heatmapWindows.append(QuotaHeatmapWindow(
                            clientId: agent.clientId, accountKey: agent.accountKey,
                            cardId: window.cardId,
                            windowLabel: window.label, total: grid.total))
                    }
                    collected.append((
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId,
                        label: window.label,
                        cycles: QuotaHistoryFold.cycles(
                            points: points)))
                }
            }
            // Deliberately not an early `return`: the per-client cycles below
            // are read through a different path and have their own error
            // handling, and suppressing them here would trade one stale surface
            // for another.
            //
            // Windows were offered, none answered, and this process has held a
            // set before: that is this pass failing to answer, not the user's
            // history ceasing to exist — see `publishedWindowSummaries`. The
            // whole publication is skipped for that one, because there is
            // nothing to publish; a thrown read is no longer handled this way
            // and is retained per window instead (#359, below).
            //
            // `windowsToRead > 0` separates this from the legitimate clear:
            // when the payload stops offering an allowance there is nothing to
            // read, and the strip should empty rather than hold the last set
            // until relaunch.
            //
            // Stated, not verified. Removing that term leaves every assertion
            // in this suite green — mutation-checked, not assumed — because the
            // no-allowance fixture is cleared through the `windowCardClients`
            // path above before reaching here. So the term is reasoning about a
            // case the fixtures do not reach, and it is kept for that reason
            // rather than because a test defends it. A payload that offers a
            // configured client with no readable window, after a set has been
            // published, is the case it is for; if that state turns out to be
            // unreachable, delete the term rather than leaving a condition
            // nothing can exercise.
            //
            // #359 changes what a thrown read costs. It used to suppress the
            // WHOLE publication: one `readFailed` and no window reached any
            // surface. That treats every throw as transient, which the comment
            // above says outright — and `antigravity` throws permanently
            // ("quota curve binding is unavailable", measured, not inferred),
            // so the suppression never lifted and the strip a process launched
            // empty stayed empty until relaunch, about history that resolves
            // fine when asked directly.
            //
            // Now a throw costs only its own window: the windows that answered
            // publish, and each window that threw keeps whatever it already
            // had. Both halves of the original intent survive — a window is
            // never dropped because we could not ask about it, and a window is
            // never replaced by an absence we did not observe — without one
            // provider's permanent failure standing in for everyone else's.
            let answeredNothing = windowsToRead > 0 && collected.isEmpty
            if !(answeredNothing && publishedWindowSummaries) {
                let fresh = QuotaOverviewFold.summaries(windows: collected)
                let freshIds = Set(fresh.map(\.id))
                // Only windows that THREW are retained. A window that answered
                // nil answered, and `answeredNothing` above is what covers the
                // case where none of them did.
                let heldOver = quotaWindowSummaries.filter {
                    failedWindowIds.contains($0.id) && !freshIds.contains($0.id)
                }
                quotaWindowSummaries = fresh + heldOver
                publishedWindowSummaries =
                    publishedWindowSummaries || !quotaWindowSummaries.isEmpty
                quotaHeatmaps = Self.retainingFailed(
                    fresh: heatmaps, previous: quotaHeatmaps, failed: failedWindowIds)
                let heldOverHeatmapWindows = quotaHeatmapWindows.filter { old in
                    failedWindowIds.contains(old.id) && !heatmapWindows.contains { $0.id == old.id }
                }
                quotaHeatmapWindows = (heatmapWindows + heldOverHeatmapWindows)
                    .sorted { $0.total > $1.total }
                // Retained for a failed window exactly as the summaries and the
                // heatmaps above are, and for a sharper reason: this dictionary
                // is what `rebuildQuotaEquivalences()` rebuilds
                // `quotaEquivalences` from, so dropping a window here removes
                // its API-value estimate from the history rows while its strip
                // and grid stay drawn. Before #359 the whole block was skipped
                // on any throw, which kept this value by accident; rebuilding
                // from `collected` alone would have turned that accident into a
                // permanent loss for a window that never reads again.
                let freshQualifying = Dictionary(
                    uniqueKeysWithValues: collected.compactMap {
                        window -> (String, QualifyingWindow)? in
                        // Capped BEFORE admitting, which is what the probe
                        // sweep measured and what actually bounds the scan:
                        // capping the admitted count instead would let 32
                        // admitted cycles span a hundred recorded ones.
                        // `collected` keeps the full list for the lifetime
                        // summaries below.
                        let admitted = QuotaHistoryFold.considered(window.cycles).filter {
                            WindowEquivalence.deltaQualifies($0.usedPercent, runs: $0.risingRuns)
                                && $0.observedFraction >= WindowEquivalence.minimumObservedFraction
                        }
                        guard admitted.count >= WindowEquivalence.minimumCycles else { return nil }
                        let identity = AccountIdentity(
                            clientId: window.clientId, accountKey: window.accountKey)
                        // An equivalence divides local usage by quota movement,
                        // and a captured Antigravity account has no local
                        // usage. Dropped here, before `refreshWindowUsage`
                        // builds one scan per qualifying account: its key is
                        // not a Claude config directory, and passing it would
                        // send it to the Claude-root lookup in
                        // `tb_window_usage`.
                        guard identity.hasLocalUsage else { return nil }
                        // Same key as the grids and the row ids — see
                        // `AccountIdentity.windowKey`. Two accounts of one
                        // client offering the same window would otherwise
                        // collide here, and `uniqueKeysWithValues` traps on a
                        // duplicate key rather than reporting it.
                        return (
                            identity.windowKey(cardId: window.cardId),
                            QualifyingWindow(
                                accountKey: window.accountKey, cycles: admitted)
                        )
                    })
                qualifyingCycles = Self.retainingFailed(
                    fresh: freshQualifying, previous: qualifyingCycles, failed: failedWindowIds)
            }
            // A window that threw and has nothing drawn is absent from the
            // strip for a reason the strip must not call "nothing recorded"
            // (#355). Outside the guard above on purpose: a pass that skips
            // publication can still be the first to fail a window that was
            // never drawn, and it must say so. "Drawn" is read from the
            // summaries as they now stand, which is the held-over set on a
            // publishing pass and the unchanged set on a skipped one.
            let drawnIds = Set(quotaWindowSummaries.map(\.id))
            quotaUnreadableClients = Set(visibleAgents.flatMap { agent in
                agent.uniqueCardWindows.compactMap { window -> String? in
                    let id = WindowCardLoader.curveKey(
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId)
                    return failedWindowIds.contains(id) && !drawnIds.contains(id)
                        ? agent.clientId : nil
                }
            })
        }

        // Cycles follow the card's client, not every displayed one: the history
        // is a per-subscription list and only the open tab shows it. The card
        // client, not the scan client: a quota-only tab has a card and history
        // and no scan.
        //
        // The client the published cycles belong to is tracked, because "keep
        // what we had" is only safe while the question has not changed. A
        // transient read failure on B used to leave A's cycles published, and
        // `QuotaView` then drew them under B's name — one subscription's
        // history labelled as another's, which is worse than an empty card and
        // indistinguishable from a correct one.
        if let client = cardClient {
            let account = cardAccountKey(for: client)
            // History vocabulary: two-part for the primary, three-part for any
            // other account, so a switch of account is a different window here.
            let selected = WindowCardLoader.selectedCardId(
                payload: agentUsage, clientId: client, accountKey: account
            ).map { WindowCardLoader.historyKey(pick: $0, accountKey: account) }
            if let cycles = WindowCardLoader.cycles(
                payload: agentUsage, clientId: client, accountKey: account,
                curve: readCurve)
            {
                quotaCycles = cycles
                quotaCyclesCardId = selected
                quotaCyclesAccount = Self.scanSlot(account)
                quotaCurveUnreadable = false
                rebuildQuotaHistory()
            } else {
                // The read failed. Whether this branch clears or retains is
                // only about a window we already had — on a FIRST failure there
                // is nothing to clear, and the two are indistinguishable in
                // state. What the card needs is the fact that the read failed,
                // which no amount of rearranging here provides: it decides on
                // `cycles.isEmpty` and `attempted`, and an unread curve is
                // empty-and-attempted exactly like a window with no history.
                quotaCurveUnreadable = true
                if quotaCyclesCardId != selected || selected == nil {
                    // Could not read, and what we hold is about a different
                    // window — another client's, or another window of this one.
                    quotaCycles = []
                    quotaHistory = []
                    quotaCyclesCardId = nil
                    quotaCyclesAccount = Self.scanSlot(account)
                }
            }
        } else {
            quotaCycles = []
            quotaHistory = []
            quotaCyclesCardId = nil
            quotaCyclesAccount = nil
            quotaCurveUnreadable = false
        }
    }

    /// Joins the cycles to the scan, whenever either changes. Cheap enough to
    /// redo rather than track: the fold is one sorted pass over the scan.
    /// Joins each qualifying window's admitted cycles to the scan.
    ///
    /// Numerators come from each cycle's OBSERVED span, not its whole window:
    /// the delta only describes the interval between two readings, and counting
    /// usage from a stretch nobody sampled against movement nobody saw is the
    /// misalignment that cost 8 points of spread on live data.
    private func rebuildQuotaEquivalences() {
        var built: [String: WindowEquivalence.Row] = [:]
        let confirmed = UsageAttribution.confirmed().records
        for (key, window) in qualifyingCycles {
            let cycles = window.cycles
            // Each row reads ITS OWN account's scan. One shared scan is what
            // issue #258 reports: every row divided every account's usage by
            // its own quota movement, which overstated the primary's estimate
            // by 1.40x and the extra account's by 3.51x on the reporting
            // machine, because the numerator was the same for both.
            guard let scan = unionScan(for: window.accountKey),
                  let client = key.split(separator: "|").first.map(String.init),
                  let oldest = cycles.last, scan.covers(start: oldest.evidenceStartMs)
            else { continue }
            // `declared` carries the empty-declaration case into the fold,
            // which is where the difference between "nothing recorded" and
            // "nothing classified" is defined.
            // The span rule and its numerators come from the same fold the
            // history card uses, sorted once and sliced per cycle. The filter
            // that used to live here re-walked the whole scan per cycle on the
            // main actor.
            let spans = QuotaHistoryFold.spans(
                cycles: cycles, messages: scan.messages, subscription: client,
                // `key` IS the window's `"<clientId>|<cardId>"`, so each
                // estimate is narrowed to its own window's scope rather than to
                // whichever one the card happens to be showing.
                modelScope: WindowCardLoader.modelScope(
                    payload: agentUsage, cardId: key),
                confirmed: confirmed)
            built[key] = WindowEquivalence.aggregate(
                // The same `client` the spans above were narrowed to. The fold
                // asks the table itself; this call site does not get to decide
                // what "declared" means.
                subscription: client, records: confirmed,
                cycles: zip(cycles, spans).map { cycle, span in
                    WindowEquivalence.Cycle(
                        deltaPercent: cycle.usedPercent, spanTokens: span.tokens,
                        spanCost: span.cost, observedFraction: cycle.observedFraction,
                        risingRuns: cycle.risingRuns)
                })
        }
        quotaEquivalences = built
    }

    /// Which window `quotaHistory` was built for, so a retained set is never
    /// shown against a different window's chart.
    @ObservationIgnored private var quotaHistoryCardId: String?

    private func rebuildQuotaHistory() {
        // Genuinely nothing to show: no window selected, or the window has no
        // cycles. Clearing is the right answer to both.
        guard let client = cardClient, let oldest = quotaCycles.last else {
            quotaHistory = []
            quotaHistoryCardId = nil
            return
        }
        // The scan has not landed, or does not reach far enough back yet. That
        // is "cannot answer", not "nothing recorded" — the same distinction
        // #359 turned on. Writing `[]` here is what made the restored rows
        // vanish on a reopen and put the card back on its placeholder, because
        // the synchronous refresh runs long before the scan returns.
        //
        // Retained only for the SAME window: the rows are annotated against
        // one window's cycles, so showing them under another is a wrong answer
        // rather than a slow one.
        // A card with no scan (quota-only tab) lists its cycles without usage
        // rows: the static scan cache is keyed by account alone, so another
        // client's scan must never be joined here.
        guard windowUsageClient == client,
              let scan = unionScan(for: cardAccountKey(for: client)),
              scan.covers(start: oldest.evidenceStartMs)
        else {
            if quotaHistoryCardId != quotaCyclesCardId {
                quotaHistory = []
                quotaHistoryCardId = nil
            }
            return
        }
        quotaHistoryCardId = quotaCyclesCardId
        quotaHistory = QuotaHistoryFold.rows(
            cycles: quotaCycles, messages: scan.messages,
            // The attribution target for a subscription's own quota is that
            // subscription's client id: the window came from the provider that
            // bills it. A row whose usage was declared elsewhere is exactly
            // what the "other" column exists to show.
            subscription: client,
            // The window these cycles belong to, not the client's default: the
            // user can select a scoped window, and its history has to answer
            // for the same allowance the chart above it draws.
            modelScope: WindowCardLoader.modelScope(
                payload: agentUsage, cardId: quotaCyclesCardId),
            confirmed: UsageAttribution.confirmed().records)
    }

    /// Whose usage bars are actually on screen, or nil on the all-agent
    /// overview, which renders no window card at all.
    ///
    /// Deliberately NOT `windowCardClients`. That set drives the quota curves,
    /// which are one read per series per publication and are wanted for every
    /// row. This one drives the message scan, and scoping the two together
    /// was a measured mistake: `copilot chat.v1` is a 31-day window, so unioning every
    /// displayed client stretched the range to 14.93 days and 109,278 messages
    /// — 67s cold — on a tab that displays none of it. The version before the
    /// two-stage split returned early here whenever no agent tab was open;
    /// this restores that bound without giving up the shared scan.
    var windowUsageClient: String? {
        // A tab with local records has both roles on the same id. Setting the
        // scan client keeps the card client in step; the popover then sets
        // `cardClient` explicitly, which is how a quota-only tab has a card
        // and no scan.
        didSet { cardClient = windowUsageClient }
    }

    /// The client whose window card, cycles and history are on screen, or nil
    /// when no card is. Separate from `windowUsageClient` (whose local messages
    /// are scanned): a quota-only tab has a card and no scan.
    var cardClient: String?

    /// Recorded reset cycles of `windowUsageClient`'s selected window, newest
    /// first. Derived from the persisted curve alone, so it lands with stage 1.
    private(set) var quotaCycles: [QuotaCycle] = []
    /// Those cycles joined to what was spent in them. Empty until the scan
    /// lands; the card shows the cycles with a loading row in the meantime,
    /// because the quota half is honest on its own and waiting for the usage
    /// half would hide it for seconds.
    private(set) var quotaHistory: [QuotaHistoryRow] = []
    /// Recorded-cycle summaries for EVERY displayed client's windows, for the
    /// all-agent lens. Curve reads only, so unlike `quotaHistory` this needs no
    /// message scan and is safe on the landing view.
    /// Which window's cycles `quotaCycles` currently holds — client AND card,
    /// not client alone. Two window selections share a client, so a client-only
    /// comparison could keep the previous window's history beneath the newly
    /// selected card. Nil when empty.
    @ObservationIgnored private(set) var quotaCyclesCardId: String?

    /// Whether the persisted curve could not be READ, as distinct from having
    /// no history in it.
    ///
    /// `QuotaHistoryCard` decides on `cycles.isEmpty` and `attempted`, and an
    /// unreadable curve is empty-and-attempted exactly like a window that has
    /// simply not accumulated anything — so it stated "No earlier windows
    /// recorded yet" about a curve it had failed to open, and kept stating it
    /// until some later refresh happened to retry. The same distinction the
    /// usage half already draws with `windowScanFailedClients`; the quota half
    /// had no way to say it.
    private(set) var quotaCurveUnreadable = false

    /// Whether a non-empty strip set has ever been published in this process.
    ///
    /// Quota history accumulates; it does not vanish between two refreshes. So
    /// once these summaries have held windows, a later refresh that produces
    /// none is not the user having no history — it is this pass failing to
    /// answer, through a route that raised nothing. `readFailed` covers a
    /// thrown read; a read that succeeds and returns nil does not set it,
    /// deliberately, because a genuinely new window has no curve yet.
    ///
    /// Both readings are correct and they are indistinguishable at the read
    /// itself. What separates them is whether there was ever anything to lose,
    /// which only this flag knows. A fresh install has never published, so an
    /// empty result publishes and the card says nothing is recorded, which is
    /// true there.
    ///
    /// The cost is bounded and stated: if a store really is emptied while the
    /// app runs, the strip keeps showing the last good set until relaunch. That
    /// is a stale reading of something that existed, against a false claim that
    /// it never did.
    private var publishedWindowSummaries = false

    private(set) var quotaWindowSummaries: [QuotaWindowSummary] = []
    /// Clients with a window whose curve read threw and that has no summary
    /// in `quotaWindowSummaries` to show for it. The strip's counterpart of
    /// `quotaCurveUnreadable`: without it an empty strip reads the same whether
    /// the history is empty or could not be opened (#355).
    private(set) var quotaUnreadableClients: Set<String> = []
    /// One weekday-by-hour grid per window, keyed as `QuotaWindowSummary.id`.
    /// Written in the same guarded block as the summaries, so it cannot be
    /// blanked by a refresh that saw no clients either.
    private(set) var quotaHeatmaps: [String: QuotaHeatmap] = [:]
    /// Every window that has a grid, heaviest first. Separate from the
    /// summaries because a window can have movement in its running cycle and no
    /// completed history at all.
    private(set) var quotaHeatmapWindows: [QuotaHeatmapWindow] = []
    /// Per-window quota equivalence, keyed `"<clientId>|<cardId>"`. Only windows
    /// with enough admitted cycles appear. Empty until the scan lands — the
    /// strips above it are free and must not wait for this.
    private(set) var quotaEquivalences: [String: WindowEquivalence.Row] = [:]
    /// Set when the Quota lens is open on the all-agent tab. Gates a scan that
    /// serves one line about one window, measured at 6.3 days and 8.1s, so it
    /// runs only when that line is actually on screen.
    var quotaLensAllAgents = false
    /// Cycles per qualifying window, kept from stage 1 so the scan can be
    /// scoped to exactly what an estimate needs and no further.
    /// One statement of #359's retention rule for every dictionary-keyed quota
    /// surface: a window that threw keeps the value it already had, and only
    /// when this pass produced nothing for it.
    ///
    /// Shared rather than written twice. The rule was stated separately for
    /// `quotaHeatmaps` and `qualifyingCycles`, in the same shape, and a rule
    /// with two homes is a rule that a later change applies to one of them —
    /// the failure this codebase has already paid for elsewhere.
    ///
    /// `quotaWindowSummaries` and `quotaHeatmapWindows` are arrays keyed by an
    /// `id` rather than dictionaries, so they cannot use this and state the
    /// same rule in their own shape. That is the remaining duplication and it
    /// is deliberate: unifying it would mean rekeying two published surfaces to
    /// make a four-line filter shorter.
    private static func retainingFailed<Value>(
        fresh: [String: Value], previous: [String: Value], failed: Set<String>
    ) -> [String: Value] {
        fresh.merging(previous.filter { failed.contains($0.key) && fresh[$0.key] == nil }) {
            fresh, _ in fresh
        }
    }

    @ObservationIgnored private var qualifyingCycles: [String: QualifyingWindow] = [:]

    /// Test seam. `qualifyingCycles` stays private because nothing outside this
    /// type may write it, but #359's retention is only observable here: the
    /// window it protects is the one whose curve cannot be read, so the
    /// equivalence estimate it feeds cannot be rebuilt to check it indirectly.
    ///
    /// Deliberately NOT behind `#if DEBUG`. The bundled selftest builds in
    /// release, and a seam compiled out there is a main-red release workflow
    /// rather than a skipped assertion — see the same note on `DiscordIPC`.
    var qualifyingCycleKeysForTesting: [String] { qualifyingCycles.keys.sorted() }

    /// One window that has enough admitted history to produce an estimate,
    /// plus the account it belongs to.
    ///
    /// The account is carried rather than parsed back out of the dictionary
    /// key. The key is an `AccountIdentity.windowKey`, whose whole point is
    /// that the primary and an extra account spell it with different segment
    /// counts; re-deriving the account from it here would be a second parser
    /// for a fact the builder already had in hand, and the two would drift.
    struct QualifyingWindow {
        let accountKey: String?
        let cycles: [QuotaCycle]
    }

    /// Identifies the newest window scan any model has started.
    ///
    /// Static, because what it guards is static. `PopoverView` builds a new
    /// `DashboardModel` on every reopen, so an instance-local counter gives the
    /// abandoned model and its replacement the same value of 1 and the guard
    /// passes for both — the token's scope has to match `lastUnionScan`'s, or
    /// it protects nothing across a reopen, which is exactly when a scan is
    /// most likely to be abandoned mid-flight.
    ///
    /// `LiveUsageDataSource.windowUsage` hands the blocking FFI call to a
    /// detached task, so cancelling the SwiftUI task that awaits it does not
    /// stop it — the result still arrives. Without this, switching tabs or
    /// lenses mid-scan lets an older, narrower request land after a newer one
    /// and overwrite `unionScan` with a stale message set stamped `capturedAt`
    /// now, which then serves as fresh for the whole 30s window.
    @ObservationIgnored private static var windowScanToken = 0

    /// Engine window scans running right now. The header spinner reads it:
    /// stage 2 is never awaited by the poll that starts it, so without this the
    /// spinner stopped and the window card and its history filled in seconds
    /// later with nothing on screen saying work was still going.
    private(set) var windowScansInFlight = 0

    private func scanWindow(accountKey: String?, from: Int64, until: Int64) async throws
        -> WindowUsage
    {
        windowScansInFlight += 1
        defer { windowScansInFlight -= 1 }
        return try await source.windowUsage(accountKey: accountKey, from: from, until: until)
    }

    /// Stage 2. One scan for the open agent tab, which every card for that
    /// client then filters from in memory.
    func refreshWindowUsage() async {
        Self.windowScanToken &+= 1
        let scanToken = Self.windowScanToken
        let now = Int64(Date().timeIntervalSince1970 * 1000)
        // Two callers, two scopes. An open agent tab needs its own window and
        // its history; the all-agent Quota lens needs only the windows that can
        // actually produce an estimate, which on live data is one of six.
        let equivalenceStart = quotaLensAllAgents
            ? qualifyingCycles.values.compactMap { $0.cycles.last?.evidenceStartMs }.min() : nil
        guard let client = windowUsageClient else {
            guard let from = equivalenceStart else { return }
            // An inverted range is a fault upstream, not an empty window. The
            // engine answers `from >= until` with an empty list rather than an
            // error (`get_window_usage`, engine PR #27), so without this the
            // loop below would cache an empty `UnionScan` and every estimate
            // drawn from it would read as "this account spent nothing" for a
            // span that was never scanned. `from` is the oldest qualifying
            // cycle's `evidenceStartMs` and the bound is `now`, so reaching
            // here means a cycle claims to start in the future.
            guard from < now else {
                // Rebuild before leaving: a previously published equivalence
                // for this window key would otherwise stay on the lens next to
                // the newly refreshed future-dated cycles, which is a stale row
                // presented as current. The normal path below ends in the same
                // call for the same reason.
                rebuildQuotaEquivalences()
                // The all-agent lens completes here and never reaches the call
                // at the foot of this function, so without this its rebuilt
                // equivalences never enter the reopen cache — the per-client
                // path was fixed and this one was not.
                refreshSnapshotLiveData()
                return
            }
            // One scan per account with a qualifying window, not one scan for
            // all of them. Each account's estimate divides its own usage by its
            // own quota movement; a shared scan is the conflation issue #258
            // reports. The accounts partition the roots between them, so this
            // is the same files read once each rather than N passes over
            // everything — measured at 1232ms + 424ms against 2130ms for the
            // single wide scan it replaces.
            //
            // Every non-nil key here is a Claude config directory: accounts
            // without local usage (captured Antigravity accounts) never enter
            // `qualifyingCycles` — see `AccountIdentity.hasLocalUsage` where
            // it is built.
            let accounts = Set(qualifyingCycles.values.map { Self.scanSlot($0.accountKey) })
            var scanned = false
            for slot in accounts {
                let accountKey = slot.isEmpty ? nil : slot
                if let cached = unionScan(for: accountKey), cached.covers(start: from),
                   Date().timeIntervalSince(cached.capturedAt) < Self.unionScanMaxAge
                { continue }
                // Deliberately does NOT touch `windowScanFailedClients`: this
                // branch serves the all-agent equivalence, which draws no window
                // card. The first version set the shared flag here, so an
                // equivalence scan failing on the lens made every client's card
                // claim its own scan had failed the moment the user opened a tab.
                //
                // One account failing does not abandon the others: their rows
                // are independent, and a rate-limited or unreadable account
                // silently removing every other account's estimate is a worse
                // answer than the estimates it could still give.
                guard let usage = try? await scanWindow(
                    accountKey: accountKey, from: from, until: now)
                else { continue }
                guard Self.windowScanToken == scanToken else { return }
                setUnionScan(
                    UnionScan(
                        fromMs: from, untilMs: now, capturedAt: Date(),
                        messages: usage.messages, undatedCount: usage.undatedCount),
                    for: accountKey)
                scanned = true
            }
            // Rebuild even when every account was already cached: the caller
            // reached this branch because something wants the estimate now.
            _ = scanned
            rebuildQuotaEquivalences()
            // Second all-agent completion path, same reason as the first.
            refreshSnapshotLiveData()
            return
        }
        // The account the card shows, resolved once for this whole pass: it
        // picks the scan slot, the engine call and the failure flag, and they
        // must not disagree. A captured Antigravity account has no local logs
        // (`hasLocalUsage`), so asking the engine would look its key up as a
        // Claude config directory; the card says so instead (rule 6).
        let account = cardAccountKey(for: client)
        guard AccountIdentity(clientId: client, accountKey: account).hasLocalUsage
        else { return }
        let failureKey = Self.scanFailureKey(client: client, accountKey: account)
        // The history's oldest cycle CONTAINS the active window, so this widens
        // one scan rather than issuing a second. Measured 2026-08-16 on live
        // data: 5.4 days, 45,844 messages, 4.6s — an order of magnitude below
        // the 14.93-day union this replaced, and paid only on the Quota lens.
        //
        // EITHER bound is a range on its own, which is why this is a `min` over
        // whichever exist rather than a window start the cycles may widen. A
        // window resolves only when the provider gave BOTH a reset and a
        // duration, and a provider can stop giving one: Codex served `resetsAt`
        // with no `limit_window_seconds` for all three of its windows (measured
        // 2026-09-08), so `unionStart` returned nil, this function returned
        // before scanning, and no scan covering that client's cycles was ever
        // cached. `rebuildQuotaHistory` then refused to join — correctly, since
        // another client's narrower scan would under-count — and every row of
        // the history card sat on "Reading local usage…" permanently, for a
        // card that needs the cycles and not the live window at all.
        guard let from = [
            WindowCardLoader.unionStart(
                payload: agentUsage, clients: [client], nowMs: now),
            quotaCycles.last?.evidenceStartMs,
        ].compactMap({ $0 }).min() else { return }
        // Inverted range, settled before anything else can answer for it.
        // Before engine PR #27 the engine rejected `from >= until` outright and
        // the call below threw, so the card said the scan had failed; it now
        // returns an empty list, which would make the card assert zero usage
        // for a range it never looked at — the "no data" versus "could not get
        // data" conflation issue #320 and selftest V15/V16/V17 are about.
        //
        // This sits ABOVE the cached-scan branch on purpose. `UnionScan.covers`
        // tests only the lower bound, so a normal scan cached moments earlier
        // satisfies it for a future `from`, and that branch would then clear
        // the failure and render the future window as zero usage — the very
        // outcome this guard exists to prevent, reached without ever calling
        // the engine. Guarding after it left the hole open on the only path
        // that does not need a scan.
        guard from < now else {
            windowScanFailedClients.insert(failureKey)
            refreshWindowQuotaHalves()
            return
        }
        // Serve the cached scan while it still covers the range and is fresh.
        // Rescanning on every reopen was the whole complaint: the staging made
        // the wait visible, it did not make it rare.
        if let cached = unionScan(for: account), cached.covers(start: from),
           Date().timeIntervalSince(cached.capturedAt) < Self.unionScanMaxAge {
            // A fresh scan that covers this window IS an answer for it, whoever
            // ran it. Returning without clearing left the card reporting a
            // failure while the model held the very data that refutes it — and
            // refusing to rescan for another 30 seconds.
            if windowScanFailedClients.contains(failureKey) {
                windowScanFailedClients = Self.scanFailures(
                    windowScanFailedClients, resolvedBy: failureKey)
                refreshWindowQuotaHalves()
            }
            return
        }
        // A throw here is a SETTLED answer, not a slow one. Returning silently
        // left the card in `.quotaOnly` with "Reading local usage…" under a
        // drawn chart for as long as the failure lasted, which is a spinner
        // that will never stop. The token check keeps an overtaken scan's
        // failure from settling a newer request, exactly as its success is.
        guard let usage = try? await scanWindow(
            accountKey: account, from: from, until: now)
        else {
            guard Self.windowScanToken == scanToken else { return }
            windowScanFailedClients.insert(failureKey)
            refreshWindowQuotaHalves()
            return
        }
        guard Self.windowScanToken == scanToken else { return }
        // Only THIS client's failure — not because the scan cannot answer for
        // another (it becomes `unionScan` on the next line, and another
        // client's own visit may then find it covering), but because coverage
        // is not tested here. A bare `nil` cleared whichever client was
        // recorded, which is neither of those things.
        windowScanFailedClients = Self.scanFailures(
            windowScanFailedClients, resolvedBy: failureKey)
        setUnionScan(
            UnionScan(
                fromMs: from, untilMs: now, capturedAt: Date(), messages: usage.messages,
                undatedCount: usage.undatedCount),
            for: account)
        refreshWindowQuotaHalves()
        rebuildQuotaHistory()
        rebuildQuotaEquivalences()
        // The scan-derived half of the lens lands here and nowhere earlier, so
        // the reopen cache has to be written after it or it carries the three
        // cards this pass just rebuilt from before they were rebuilt. The poll
        // loop's own call cannot cover this: that one runs on the quota
        // payload, and this runs on the scan — the slower of the two, and the
        // reason the history card was the last thing on screen to fill.
        refreshSnapshotLiveData()
    }

    /// The clients whose stage-two scan settled with an error.
    ///
    /// Distinguishes "still scanning" from "asked and failed" — `.quotaOnly`
    /// alone cannot, and both halves of the lens rendered the second as the
    /// first. A per-client answer, not a shared flag: a single boolean says
    /// "did the last scan fail", which is a different question from "did THIS
    /// card's scan fail", and one transient failure on the all-agent lens made
    /// the next tab the user opened claim its own scan had failed before that
    /// scan had started.
    ///
    /// A SET, not one client, for the mirror of the same reason. The
    /// single-slot version scoped its CLEAR to the client that succeeded but
    /// let its SET overwrite whichever client was recorded, so two failures in
    /// a row lost the first and its card went back to "Reading local usage…" —
    /// the never-ending spinner this state exists to remove. Half a rule is not
    /// a rule.
    ///
    /// Cleared at exactly two sites, both in `refreshWindowUsage`: the cached
    /// branch and the success path. Their conditions are deliberately NOT
    /// restated here. Four successive attempts to paraphrase them each read as
    /// precise and each omitted a conjunct — the scan-token check, the
    /// freshness bound, the fact that the cached branch tests coverage against
    /// `from` rather than against the window. Read the two sites; a summary of
    /// a conjunction is a place for one of its terms to go missing.
    ///
    /// One thing worth stating because it is counter-intuitive: `unionScan` is
    /// cross-client in PROVENANCE, not in effect. A scan run for A can clear B,
    /// but only when B itself visits, and only B.
    ///
    /// Deliberately not cleared when a retry STARTS: the card would then flip
    /// failed → loading → failed on every poll, and the last settled answer is
    /// more useful than a spinner that keeps restarting. During a retry the
    /// card still says the last attempt failed, which is true and is the
    /// intended reading.
    private(set) var windowScanFailedClients: Set<String> = []

    /// Whether the card currently shown for `clientId` should say the scan
    /// failed rather than that it is still running.
    func windowScanFailed(for clientId: String) -> Bool {
        windowScanFailed(for: clientId, accountKey: cardAccountKey(for: clientId))
    }

    /// For one account's scan slot. Flags are keyed by (client, account): a
    /// failed primary scan must not mark account B failed while B's own scan is
    /// still pending, nor B's failure mark the primary.
    func windowScanFailed(for clientId: String, accountKey: String?) -> Bool {
        windowScanFailedClients.contains(
            Self.scanFailureKey(client: clientId, accountKey: accountKey))
    }

    /// The failure-set member for one card's scan. The bare client id for the
    /// primary, so a one-account client keeps the key it always had.
    nonisolated static func scanFailureKey(client: String, accountKey: String?) -> String {
        accountKey.map { "\(client)\u{1F}\($0)" } ?? client
    }

    /// The recorded failures after a scan for `client` succeeds.
    ///
    /// Static and pure so the rule can be asserted on: an earlier version
    /// assigned `nil` unconditionally, so a success on one client erased
    /// another's recorded failure while the doc comment claimed the opposite.
    /// Nothing in a view or an async method could catch that.
    nonisolated static func scanFailures(
        _ current: Set<String>, resolvedBy client: String
    ) -> Set<String> {
        current.subtracting([client])
    }

    private func reconcileQuotaRemaining(with payload: AgentUsagePayload) {
        guard source.allowsQuotaCachePersistence else { return }
        let defaults = UserDefaults.standard
        _ = TrayAnimator.applyQuotaRemaining(
            payload: payload,
            persistedSelection: defaults.string(forKey: TrayAnimator.quotaSourceKey)
                ?? QuotaResolver.auto,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: defaults.object(forKey: TrayAnimator.lastRemainingKey) as? Double,
            defaults: defaults)
    }

    /// Poll the OAuth quota snapshots while the popover is open. The fetch is
    /// network-bound (up to ~30s when a provider hangs), so failures keep the
    /// previous payload; per-provider errors live inside each snapshot.
    func pollAgentUsage() async {
        while !Task.isCancelled {
            // Read BEFORE the fetch. The fetch is network-bound and owns most
            // of the cycle, so a registry change lands during it far more often
            // than during the sleep; carrying the epoch across is what stops
            // that change being dropped.
            let registryEpoch = ClaudeExtraRoots.RegistryChange.epoch
            // A login change in agy must be seen before this fetch, or its
            // payload is drawn as the previous account (see `prepareForFetch`).
            await TrayAnimator.prepareAntigravityAutoCapture()
            if Task.isCancelled { break }
            let payload = try? await source.agentUsage()
            if Task.isCancelled { break }
            // Same guard the tray poll carries, for the same reason and in the
            // same shape: a payload built for the previous account set is not
            // an answer to the question this registry asks. `continue` rather
            // than falling through, so the immediate refetch happens now
            // instead of after the sleep — and `agentUsageAttempted` stays
            // where it was, because nothing about the new registry has settled.
            if ClaudeExtraRoots.RegistryChange.epoch != registryEpoch { continue }
            if let payload {
                let resolved = AgentUsagePublicationCoordinator.resolve(payload)
                agentUsage = resolved
                reconcileQuotaRemaining(with: resolved)
                // Stage 1 is synchronous and lands with the payload; stage 2
                // is kicked off without being awaited, so the poll loop never
                // holds the card behind a scan.
                refreshWindowQuotaHalves()
                // AFTER the refresh, not before it. The cache now carries the
                // published strip as well as the payload, and capturing it
                // first stored the PREVIOUS pass's strip every time — empty on
                // a model's first poll, so the reopen this restore exists for
                // would still have drawn nothing. Both values this writes are
                // ready by here: `agentUsage` is assigned above, and the strip
                // is what the line before just published.
                refreshSnapshotLiveData() // keep the reopen cache's quota cards current
                Task { await refreshWindowUsage() }
            }
            // Set on failure too: `agentUsage == nil` alone cannot distinguish
            // "the first attempt is still in flight" from "the attempt finished
            // and produced nothing", and UI that waits on the payload would spin
            // forever against a persistent failure.
            let firstSettlement = !agentUsageAttempted
            agentUsageAttempted = true
            // And recompute the cards on that first failure, because
            // `refreshWindowQuotaHalves` is the only writer of `windowCards`
            // and it was called from the success branch alone. Teaching
            // `quotaHalf` to answer `.blocked` once the attempt has settled did
            // nothing on its own: with nothing else writing that state, the
            // card kept the `.loading` it was built with until a tab change
            // rebuilt it. Only on the transition, so a run of failures does not
            // redo the same work every minute.
            if payload == nil, firstSettlement { refreshWindowQuotaHalves() }
            // Interruptible: adding or removing an account must take its card
            // with it in the same turn, not at this loop's convenience.
            await ClaudeExtraRoots.RegistryChange.sleep(upTo: 60, since: registryEpoch)
        }
    }

    /// Poll the live tail (10-minute window) — drives the limits card's
    /// "Live" badge now and the trace card in a later phase. The staticlib
    /// re-parses at most every 10s, so this matches its cadence.
    func pollTrace() async {
        while !Task.isCancelled {
            let buckets = try? await source.usageTrace(windowSecs: 600)
            if Task.isCancelled { break }
            if let buckets {
                trace = buckets
                refreshSnapshotLiveData() // keep the reopen cache's live trace current
            }
            try? await Task.sleep(for: .seconds(10))
        }
    }

    /// Fetch the lazy per-lens reports on first activation — and, because
    /// PopoverView's `.task` is keyed on the year too, again for the active
    /// lens after a year switch. Re-checks the year after the off-actor fetch
    /// (mirrors load()/reload()/pollGraph()): a year change mid-fetch drops the
    /// stale slice instead of stranding the previous year's report on the lens,
    /// and the keyed `.task` re-fires to fetch the new year while the report is
    /// still nil (reload()'s lazy re-fetch only covers an already-loaded lens).
    /// `clients` is the active tab's slice (displayClients on Overview,
    /// `[clientId]` on a client tab). It is threaded to the FFI so hourly/agents
    /// totals are accurate for hours/agents shared across clients. Refetches
    /// when the slice changes, not only when the report is nil — keyed on the
    /// slice as a Set so a reorder does not refetch. The year stale-guard
    /// mirrors reload()/pollGraph().
    private func ensureHourlyData(
        year: String?, clients: [String]
    ) async {
        let selection = Set(clients)
        let yearKey = Self.identityYear(year)
        guard hourly == nil || hourlyClients != selection || hourlyYear != yearKey else { return }

        // Restore the exact slice before refreshing so reopen and lens switches
        // do not flash a loading state. Non-popover models neither read nor
        // write this process cache.
        let cacheKey = HourlyCacheKey(year: yearKey, clients: selection)
        if cachesSnapshot, let cached = Self.hourlyCache[cacheKey] {
            publishHourly(cached, year: year, clients: selection)
        } else if hourly != nil {
            hourly = nil
            hourlyClients = nil
            hourlyYear = nil
        }
        let requestToken = beginHourlyRequest()
        let report = try? await source.hourlyReport(
            year: year, clients: clients, priority: .userInitiated)
        guard self.year == year, hourlyRequestToken == requestToken else { return }
        guard !Task.isCancelled, let report else { return }
        publishHourly(report, year: year, clients: selection)
    }

    /// Fetch the model report for a lens that needs it, once the graph has
    /// committed. Idempotent per (year, payload generation): a lens switch or a
    /// re-render does not re-request, while a graph refresh that actually moved
    /// the payload does. A failure keeps the last-good report rather than
    /// blanking the card.
    /// True when the committed payload describes the slice `year` currently
    /// selects — the only state in which a model scan is worth issuing. False
    /// on first paint (no payload yet) and mid-year-switch.
    private var modelSliceIsCommitted: Bool {
        payload != nil && acceptedPayloadYear == Self.identityYear(year)
    }

    /// Whether the last graph fetch threw without committing. Set inside the
    /// gated task, so a waiter reading it after the gate opens sees the same
    /// answer the fetch's owner does; cleared by the next commit.
    ///
    /// `tb_model_report` takes a year and nothing else: it always scans current
    /// logs, and the payload generation is a cache identity rather than a
    /// filter. So when the fetch fails, `try?` leaves the RESTORED payload
    /// standing and a scan issued against it puts a reading of now beside a
    /// chart from the previous popover session — hours apart, tagged as if they
    /// agreed. Before the graph/model split this could not happen: `load()`
    /// awaited both, and a throwing graph discarded the model with it.
    ///
    /// This is not the ordinary skew of two independent scans. `tb_graph`
    /// serves a payload up to `ONESHOT_MAX_AGE_SECS` old while the model report
    /// is uncached, so a 30-second lead is normal and bounded; the failure path
    /// is bounded only by how long the graph keeps failing.
    ///
    /// A restored payload with a fetch still IN FLIGHT is not this state — that
    /// one is what waiting on the gate is for.
    @ObservationIgnored private var graphFetchFailed = false
    /// Issued per graph fetch so an obsolete one cannot report failure over a
    /// newer commit. `load()` for year A can still be in flight when the user
    /// picks year B — the existing phantom-slice guards exist for exactly that
    /// overlap — and if B commits first, A's later failure would otherwise mark
    /// the displayed slice failed and strand its model cards on a spinner until
    /// the next successful poll. Release by ownership, not by value: the same
    /// rule `modelRequestToken` follows.
    @ObservationIgnored private var graphFetchToken = 0

    private func ensureModelReport(priority: TaskPriority) async {
        modelWanted = true
        // Raise the flag BEFORE waiting. A restored snapshot is already
        // `.ready`, so the model cards are on screen for the whole wait below —
        // and with the flag down they render "No model usage in this range",
        // reporting a deferred read as an answered one. Only when nothing is
        // displayable: a last-good report stays visible unflagged.
        if modelReport == nil { modelLoading = true }
        // LP3 restore gate. A restored snapshot (memory or disk) without a
        // model report current for it makes `modelSliceIsCommitted` below
        // true from the FIRST body evaluation — payload present, year
        // matching — while `graphLoadTask` is still nil, because `load()`
        // may not even have called `gatedGraph` yet. Without this wait, a
        // model-task-first ordering would fall straight through the guard
        // below and the `while` loop after it (nothing "in flight" to wait
        // on) and scan against a payload nobody has confirmed live. Awaiting
        // it here is a no-op unless `init` installed it — see
        // `restoreGatePending`'s doc comment — and it resolves as soon as
        // ANY graph fetch settles, whether that is `load()`, `reload()`, or
        // `pollGraph()`.
        await waitForRestoreGate()
        // Settle the slice BEFORE waiting on anything. `setYear` moves `year`
        // synchronously while the payload only catches up when reload commits,
        // and PopoverView's task id contains both — so this can be entered with
        // a phantom slice (new year, previous payload). There is nothing to
        // wait for in that state: the answer for the requested slice cannot
        // exist until the reload commits, and apply() re-fires this task the
        // moment it does. Parking behind the gate instead would hold the caller
        // behind the very fetch that invalidates its question — and, since
        // `setYear` installs that gate synchronously, would deadlock a caller
        // that awaits this directly.
        //
        // The report is absent but the answer is not "none" — leaving the flag
        // down here let the cards render their empty copy for the whole graph
        // reload, telling the user a year has no model usage while it was
        // still being read.
        guard modelSliceIsCommitted else {
            modelLoading = true
            return
        }
        // Wait for a graph fetch already running rather than scanning beside
        // it. A restored snapshot hands this task a real key on the first body
        // evaluation, so on every popover reopen it would otherwise start while
        // `load()` is still scanning — putting both back on the bounded pool,
        // which is the contention this separation exists to remove. Read the
        // payload only after, since the fetch may have committed a new one.
        // Follow the chain, not just the task in hand. A fetch can be
        // superseded while this waits, and a superseded one commits nothing —
        // resuming on it would scan against the payload the newer fetch is
        // about to replace, back on the same bounded pool. Terminates because
        // each turn awaits a strictly newer task, and only a fresh
        // `gatedGraph` call can install one.
        while let inFlight = graphLoadTask {
            _ = try? await inFlight.value
            if graphLoadTask == inFlight { break }
        }
        guard let payload, modelSliceIsCommitted, !graphFetchFailed else {
            modelLoading = true
            return
        }
        let year = self.year
        let generation = payload.meta.generatedAt
        let identity = ModelSliceIdentity(year: Self.identityYear(year), generation: generation)
        if modelReport != nil,
           modelYear == identity.year,
           modelPayloadGeneratedAt == generation,
           Self.isFresh(modelFetchedAt: modelFetchedAt)
        {
            // A report can land from another task while this one waits on the
            // graph gate above, so the flag raised there has to come back down
            // — nothing is in flight to lower it later.
            modelLoading = false
            return
        }
        // Coalesce re-entry onto the request already running for this exact
        // slice. Every entry bumps the request token, so a second one would
        // strand the first — the earlier and likely sooner-finishing scan —
        // and put two full FFI scans on the same bounded pool. The triggers are
        // ordinary interaction: each Daily/Monthly row expand calls
        // `ensureModelColors`, and PopoverView's model task id includes the
        // lens, so Overview→Models mid-scan re-enters too.
        // Adopt the scan already running for this exact slice rather than
        // returning: the caller is a lens-keyed SwiftUI task, and switching
        // lens mid-scan cancels the one that started it. If publication rode
        // that task, the cancelled owner would discard the result and the
        // re-entrant lens would sit empty with nothing left to publish. Awaiting
        // the shared task instead means whoever is still around gets the value.
        if modelInFlight == identity, let running = modelTask {
            await running.value
            return
        }
        modelInFlight = identity
        let requestToken = beginModelRequest()
        modelLoading = true
        // Unstructured, so the fetch and its publication outlive the view task
        // that triggered them. `Task {}` does not inherit cancellation, which is
        // exactly the property needed here; `invalidateModel` cancels it
        // explicitly when the slice it belongs to stops being displayed.
        let task = Task { [source] in
            try? await source.modelReport(year: year, priority: priority)
        }
        modelTask = task
        // Publish from inside the unstructured task, not after awaiting it: the
        // caller is a lens-keyed view task, and a lens switch cancels it while
        // the scan runs. Gating publication on the caller's liveness is what
        // let a cancelled Overview task discard the report the Models lens was
        // waiting for. Slice identity — year plus request token — decides
        // whether the result is still wanted; the caller's fate does not.
        let report = await task.value
        if modelTask == task { modelTask = nil }
        // Release by OWNERSHIP, not by value. `ModelSliceIdentity` is a value,
        // so two different scans can carry an identical one: a year round-trip
        // A→B→A during a scan returns the same payload from `tb_graph`'s 30s
        // cache, hence the same generation. Comparing identities would let the
        // stale scan clear the live scan's slot, and the next ordinary
        // re-entry would start a third scan concurrent with the second. The
        // request token is unique per scan, so only its owner can release it.
        if modelRequestToken == requestToken { modelInFlight = nil }
        // A year switch or a newer request invalidates this completion: publishing
        // it would strand another slice's models on the current dashboard.
        let isCurrent = modelRequestToken == requestToken
        // Only the current request owns the flag; a superseded one must leave it
        // set for the request that replaced it, and must not strand it either.
        if isCurrent { modelLoading = false }
        guard self.year == year, isCurrent else { return }
        guard let report else { return }
        publishModel(report, year: year, generation: generation)
    }

    /// The retry `pollGraph` performs each tick. Factored out so the condition
    /// exists in ONE place: while the test hook below restated it, a mutation
    /// of the poll's own condition changed nothing the suite could observe.
    ///
    /// Gated on `modelWanted` and nothing else. A `modelReport == nil` test
    /// would restate the staleness rule a second time and get it wrong — a
    /// failure that lands while a LAST-GOOD report is displayed keeps that
    /// report, so the retry never fired and the cards sat on the previous
    /// generation's models beside an advancing chart. `ensureModelReport`
    /// already owns that judgement (year plus payload generation) and returns
    /// immediately when the report is current, so a fresh call costs nothing.
    private func retryModelIfStale(priority: TaskPriority) async {
        guard modelWanted else { return }
        await ensureModelReport(priority: priority)
    }

    /// Drives that retry without waiting 60 seconds for the loop.
    func retryMissingModelForTest() async {
        await retryModelIfStale(priority: .utility)
    }

    /// Model-dependent lenses. Separate from `ensureData` because that task is
    /// keyed on the lens/year/client slice only, so folding the model into it
    /// would start the model scan alongside the graph again — the exact
    /// contention this split removes. PopoverView keys this on the committed
    /// payload generation instead.
    ///
    /// PRECONDITION, not merely convention: `load()` must have been called on
    /// this model at least once before this is. Every production caller
    /// satisfies it (`PopoverView` always runs both `.task { load() }` and the
    /// model-data task), but nothing here enforces it — a future caller that
    /// skips `load()` on a model whose restore installed the gate above will
    /// hang forever awaiting a fetch nobody ever starts. This is a liveness
    /// trap, not a safe API to call on its own.
    func ensureModelData(for view: AppView) async {
        switch view {
        case .overview, .models, .stats:
            await ensureModelReport(priority: .userInitiated)
        // The attribution card on the Quota lens needs the report to offer
        // anything, so the lens asks for it while the card can still show.
        // Once asked, the report is kept current like on Overview
        // (`retryModelIfStale`) until the slice changes, even if the card is
        // declined or applied in the meantime: the same cost Overview always
        // pays. Nothing asks for it on Quota after that.
        case .quota where AttributionOnboardingCard.mayShow():
            await ensureModelReport(priority: .userInitiated)
        default:
            break
        }
    }

    /// Daily/Monthly render no model card, so they never fetch the report on
    /// activation — but expanding a row reveals a per-model drill-down whose
    /// dots are tinted by `ModelColorMap`, and that map is built from the model
    /// report. Before the graph/model split `apply()` populated it on every
    /// load; without this hook the drill-down would flatten every model of a
    /// provider onto the same rank-0 shade. Idempotent, so repeated expands
    /// cost nothing once the report has landed.
    ///
    /// PRECONDITION, not merely convention — same as `ensureModelData`: this
    /// reaches the same gate, so calling it on a restored model that never had
    /// `load()` called first is a future-caller liveness trap, not a safe API.
    func ensureModelColors() async {
        await ensureModelReport(priority: .userInitiated)
    }

    func ensureData(for view: AppView, clients: [String]) async {
        let year = self.year
        switch view {
        case .hourly:
            // Hourly keeps the established nil/empty = all clients contract
            // from `ctb.h`. Daily/Monthly used to opt out of it; they no longer
            // reach this call at all, since their turns ride the graph payload.
            await ensureHourlyData(year: year, clients: clients)
        case .agents:
            let selection = Set(clients)
            guard agents == nil || agentsClients != selection else { return }
            // Nil the stale report on a slice change (see the hourly case).
            if agents != nil, agentsClients != selection { agents = nil; agentsClients = selection }
            let report = try? await source.agentsReport(
                year: year, clients: clients, priority: .userInitiated)
            guard self.year == year, !Task.isCancelled else { return }
            agents = report
            agentsClients = selection
        default:
            break
        }
    }

    /// Shared async live-rate helper for PopoverView and SettingsWindowView.
    /// The source remains the only owner of raw usage calls; hidden-client
    /// filtering follows the same policy as the tray and live-session card.
    func tokensPerMin() async -> Double? {
        try? await LiveRate.current(source: source)
    }
}

// MARK: - Dashboard device selection (SA-3d)

/// Which device-aware page an overlay holds.
enum DeviceOverlayKind: Hashable, Sendable {
    case graph, models, hourly, agents
}

/// What an overlay was fetched for. A response lands only while its key is
/// still the one the lens wants; a slot is shown only while its key is the
/// one the lens wants now.
struct DeviceOverlayKey: Hashable, Sendable {
    let kind: DeviceOverlayKind
    /// "" = all time.
    let year: String
    /// Hourly and Agents only; nil = every client. Sorted and unique.
    let clients: [String]?
    let local: Bool
    /// Sorted `producerStatic` values.
    let peers: [String]

    var selection: DeviceSelection { DeviceSelection(local: local, peers: peers) }
    var yearArgument: String? { year.isEmpty ? nil : year }
}

/// One overlay: the core's merged page for `key`, kept beside (never in place
/// of) the local data. `failed`: the last refresh for this key failed; `data`
/// is then the last good answer, or nil when there never was one.
struct DeviceOverlaySlot<Payload: Decodable & Sendable>: Sendable {
    let key: DeviceOverlayKey
    var data: DeviceAwarePayload<Payload>?
    var failed: Bool
}

/// The persisted choice (`tokenbar.dashboard.deviceSelection`, JSON). Peers
/// are `producerStatic` values.
struct StoredDeviceSelection: Codable, Equatable, Sendable {
    var local = true
    var peers: [String] = []
}

private enum DeviceOverlayResult: Sendable {
    case graph(DeviceAwarePayload<UsagePayload>)
    case models(DeviceAwarePayload<ModelReport>)
    case hourly(DeviceAwarePayload<HourlyReport>)
    case agents(DeviceAwarePayload<AgentsReport>)
}

/// The device overlay of the dashboard's Graph (Stats, Daily, Monthly), Models,
/// Hourly and Agents lenses.
///
/// Isolation, by construction: this type holds the core's merged pages and
/// nothing else. It has no access to `DashboardModel`'s private state, and
/// `DashboardModel` never reads these values into `apply(payload:)`, `payload`,
/// `stats`, `knownYears`, `tokenbar.localRecordClients`, `modelReport`,
/// `colors`, `hourly`, `hourlyCache`, `agents`, the snapshot or the tray's
/// `lastGraph`. Merging across sources happens in the core
/// (`remote_pages.rs`); nothing here adds, subtracts or splices a local value
/// with a remote one. What it derives is from one overlay payload at a time
/// (`UsageStats`, client slices, `ModelColorMap`), with its own instances.
@MainActor @Observable final class DeviceOverlayState {
    static let selectionKey = "tokenbar.dashboard.deviceSelection"
    /// How often the lens on screen is refreshed from the producers. A producer
    /// grants a burst of 2 requests and refills one per 60 s, and the Stats
    /// lens asks for two pages per refresh, so a faster poll would only be
    /// answered with last-good data and a refusal.
    static let remotePollSeconds: Double = 300
    /// The core refuses more than 16 producers in one call.
    static let maxSelectedPeers = 16
    private static let order: [DeviceOverlayKind] = [.graph, .models, .hourly, .agents]

    @ObservationIgnored private let source: any UsageDataSource
    @ObservationIgnored private let defaults: UserDefaults

    private(set) var stored: StoredDeviceSelection
    /// Producers the last successful `remotePeers()` listed as active. Empty
    /// when the list could not be read: the menu hides and only this Mac's own
    /// path runs.
    private(set) var selectablePeers: [RemotePeer] = []
    private(set) var graph: DeviceOverlaySlot<UsagePayload>?
    private(set) var models: DeviceOverlaySlot<ModelReport>?
    private(set) var hourly: DeviceOverlaySlot<HourlyReport>?
    private(set) var agents: DeviceOverlaySlot<AgentsReport>?
    /// Of the graph overlay's own payload, as `apply` builds it for the local one.
    private(set) var graphStats: UsageStats?
    /// Of the models overlay's own report.
    private(set) var modelsColors = ModelColorMap(report: nil)

    @ObservationIgnored private var wanted: [DeviceOverlayKind: DeviceOverlayKey] = [:]
    /// The overlay's own memo for a client slice of `graphStats`; not
    /// `DashboardModel.stats(selecting:)`'s, which keys on `generatedAt` and
    /// would collide with the local payload's.
    @ObservationIgnored private var statsMemo: (key: DeviceOverlayKey, selected: Set<String>, value: UsageStats)?

    init(source: any UsageDataSource, defaults: UserDefaults = .standard) {
        self.source = source
        self.defaults = defaults
        stored = defaults.data(forKey: Self.selectionKey)
            .flatMap { try? JSONDecoder().decode(StoredDeviceSelection.self, from: $0) }
            ?? StoredDeviceSelection()
    }

    // MARK: Selection

    /// The selection that reaches the core, or nil when only this Mac is
    /// chosen (or the peer list is unknown): then no `*Devices` call is made.
    var effective: DeviceSelection? {
        let known = Set(selectablePeers.map(\.producerStatic))
        let chosen = stored.peers.filter(known.contains).sorted().prefix(Self.maxSelectedPeers)
        return chosen.isEmpty ? nil : DeviceSelection(local: stored.local, peers: Array(chosen))
    }

    var includesRemote: Bool { effective != nil }

    /// Changes whenever the lens has to ask again.
    var signature: String {
        effective.map { "\($0.local)|\($0.peers.joined(separator: ","))" } ?? "-"
    }

    /// Sources currently chosen among those that can be chosen.
    var selectedCount: Int {
        let known = Set(selectablePeers.map(\.producerStatic))
        return (stored.local ? 1 : 0) + stored.peers.filter(known.contains).count
    }

    func isSelected(peer: String) -> Bool { stored.peers.contains(peer) }

    /// Chosen devices that can be chosen (not counting this Mac).
    var selectedPeerCount: Int {
        let known = Set(selectablePeers.map(\.producerStatic))
        return stored.peers.filter(known.contains).count
    }

    /// The last selected source cannot be unchecked, and no more than
    /// `maxSelectedPeers` devices can be checked: `effective` would drop the
    /// extra one silently, so it would look checked and not be asked.
    func canToggle(local: Bool, peer: String? = nil) -> Bool {
        let selected = peer.map(isSelected(peer:)) ?? stored.local
        if selected { return selectedCount > 1 }
        return peer == nil || selectedPeerCount < Self.maxSelectedPeers
    }

    func toggleLocal() {
        guard canToggle(local: true) else { return }
        stored.local.toggle()
        selectionChanged()
    }

    func togglePeer(_ id: String) {
        guard canToggle(local: false, peer: id) else { return }
        if let index = stored.peers.firstIndex(of: id) {
            stored.peers.remove(at: index)
        } else {
            stored.peers.append(id)
        }
        selectionChanged()
    }

    private func selectionChanged() {
        persist()
        if !includesRemote { clearOverlays() }
    }

    private func persist() {
        if let data = try? JSONEncoder().encode(stored) {
            defaults.set(data, forKey: Self.selectionKey)
        }
    }

    /// Back to this Mac alone: every overlay goes, and a response still in
    /// flight finds nothing wanted.
    private func clearOverlays() {
        wanted = [:]
        graph = nil; models = nil; hourly = nil; agents = nil
        graphStats = nil
        modelsColors = ModelColorMap(report: nil)
        statsMemo = nil
    }

    // MARK: Peers

    /// Reads the peer list on a thread of its own. Only a successful read can
    /// drop a stored peer that is no longer listed; a failed one leaves the
    /// stored choice alone and hides the menu, so a stopped agent does not
    /// forget the user's selection.
    func refreshPeers() {
        let source = self.source
        Task {
            let result = await Self.onOwnThread { try source.remotePeers() }
            self.peersArrived(result)
        }
    }

    private func peersArrived(_ result: Result<[RemotePeer], Error>) {
        switch result {
        case .success(let list):
            selectablePeers = list.filter { $0.state == "active" }
            let known = Set(selectablePeers.map(\.producerStatic))
            let kept = stored.peers.filter(known.contains)
            if kept != stored.peers {
                stored.peers = kept
                if kept.isEmpty { stored.local = true }
                persist()
            }
        case .failure:
            selectablePeers = []
        }
        if !includesRemote { clearOverlays() }
    }

    // MARK: Fetching

    static func kinds(for view: AppView) -> [DeviceOverlayKind] {
        switch view {
        case .daily, .monthly: return [.graph]
        case .stats: return [.graph, .models]
        case .models: return [.models]
        case .hourly: return [.hourly]
        case .agents: return [.agents]
        case .overview, .quota: return []
        }
    }

    /// Hourly's and Agents' `clients` argument. A single-client tab sends its
    /// slice. Overview sends nil while nothing is hidden, else the registry's
    /// ids minus the hidden ones (not this Mac's present list: a client only a
    /// remote device used must still be asked for).
    static func clientsArgument(singleClient: String?, hidden: Set<String>) -> [String]? {
        if let singleClient { return ClientRegistry.tabSlice(singleClient) }
        let hiddenAll = ClientRegistry.hiddenTabClients(hidden)
        return hiddenAll.isEmpty ? nil : ClientRegistry.allIds.filter { !hiddenAll.contains($0) }
    }

    func key(
        _ kind: DeviceOverlayKind, year: String?, clients: [String]?
    ) -> DeviceOverlayKey? {
        guard let selection = effective else { return nil }
        let carriesClients = kind == .hourly || kind == .agents
        return DeviceOverlayKey(
            kind: kind, year: year ?? "",
            clients: carriesClients ? clients.map { Array(Set($0)).sorted() } : nil,
            local: selection.local, peers: selection.peers)
    }

    /// Declares what the lens on screen needs and starts the loads: for each
    /// page without an overlay for its key, one `cached` call (shown at once),
    /// then a `refresh`. Overview and Quota need none. Safe to call on every
    /// change of lens, year, client slice or selection.
    func ensure(view: AppView, year: String?, clients: [String]?) {
        var next: [DeviceOverlayKind: DeviceOverlayKey] = [:]
        for kind in Self.kinds(for: view) {
            if let key = key(kind, year: year, clients: clients) { next[kind] = key }
        }
        wanted = next
        let keys = Self.order.compactMap { next[$0] }
        guard !keys.isEmpty else { return }
        let missing = keys.filter { currentKey(of: $0.kind) != $0 }
        guard !missing.isEmpty else { return scheduleRefresh(keys) }
        Task {
            for key in missing { await self.runOnce(key, .cached) }
            // Only while these are still the keys on screen: a stale request
            // would replace a newer pending one, which `finishRefresh` then
            // drops, leaving the newest lens without its refresh.
            guard keys.allSatisfy({ self.wanted[$0.kind] == $0 }) else { return }
            self.scheduleRefresh(keys)
        }
    }

    /// The popover closed: nothing is on screen, so a refresh in flight stops
    /// issuing its remaining pages and a pending one is skipped. The slots
    /// stay. A reopened popover calls `ensure` again (its `.task(id:)`).
    func popoverClosed() { wanted = [:] }

    private func currentKey(of kind: DeviceOverlayKind) -> DeviceOverlayKey? {
        switch kind {
        case .graph: graph?.key
        case .models: models?.key
        case .hourly: hourly?.key
        case .agents: agents?.key
        }
    }

    /// The poll's tick: refresh the lens on screen. A tick while a refresh
    /// runs is skipped.
    func pollOnce() {
        guard effective != nil, !Self.refreshInFlight, !wanted.isEmpty else { return }
        startRefresh(Self.order.compactMap { wanted[$0] })
    }

    func manualRefresh() {
        guard effective != nil, !wanted.isEmpty else { return }
        scheduleRefresh(Self.order.compactMap { wanted[$0] })
    }

    /// Mounted by the popover, so it ends with it.
    func pollLoop() async {
        while !Task.isCancelled {
            try? await Task.sleep(for: .seconds(Self.remotePollSeconds))
            if Task.isCancelled { break }
            pollOnce()
        }
    }

    // One `refresh` at a time in the whole process: a refresh can hold the
    // agent's read lock for up to 145 s per producer, and a second one would
    // only queue behind the first on the core's own mutex. A trigger during a
    // refresh keeps only the newest request and runs it once afterwards.
    private struct PendingRefresh {
        weak var owner: DeviceOverlayState?
        var keys: [DeviceOverlayKey]
    }
    private static var refreshInFlight = false
    private static var pendingRefresh: PendingRefresh?

    /// Test seam: whether a refresh is running anywhere in the process.
    static var refreshInFlightForTesting: Bool { refreshInFlight }

    /// Test seam: forget the process-wide guard between cases.
    static func resetSingleFlightForTesting() {
        refreshInFlight = false
        pendingRefresh = nil
    }

    private func scheduleRefresh(_ keys: [DeviceOverlayKey]) {
        if Self.refreshInFlight {
            Self.pendingRefresh = PendingRefresh(owner: self, keys: keys)
            return
        }
        startRefresh(keys)
    }

    private func startRefresh(_ keys: [DeviceOverlayKey]) {
        Self.refreshInFlight = true
        Task {
            for key in keys { await self.runOnce(key, .refresh) }
            Self.finishRefresh()
        }
    }

    private static func finishRefresh() {
        refreshInFlight = false
        guard let next = pendingRefresh else { return }
        pendingRefresh = nil
        if let owner = next.owner, next.keys.allSatisfy({ owner.wanted[$0.kind] == $0 }) {
            owner.startRefresh(next.keys)
        }
    }

    private func runOnce(_ key: DeviceOverlayKey, _ mode: DeviceFetchMode) async {
        guard wanted[key.kind] == key else { return }
        let source = self.source
        let result = await Self.onOwnThread { try Self.fetch(source, key, mode) }
        guard wanted[key.kind] == key else { return }
        store(result, for: key, mode: mode)
    }

    nonisolated private static func fetch(
        _ source: any UsageDataSource, _ key: DeviceOverlayKey, _ mode: DeviceFetchMode
    ) throws -> DeviceOverlayResult {
        let (year, selection) = (key.yearArgument, key.selection)
        switch key.kind {
        case .graph:
            return .graph(try source.graphDevices(year: year, selection: selection, mode: mode))
        case .models:
            return .models(
                try source.modelReportDevices(year: year, selection: selection, mode: mode))
        case .hourly:
            return .hourly(try source.hourlyReportDevices(
                year: year, clients: key.clients, selection: selection, mode: mode))
        case .agents:
            return .agents(try source.agentsReportDevices(
                year: year, clients: key.clients, selection: selection, mode: mode))
        }
    }

    /// A cached miss changes nothing (a refresh follows); a refresh miss keeps
    /// the last good answer for this key and says so.
    private func store(_ result: Result<DeviceOverlayResult, Error>, for key: DeviceOverlayKey, mode: DeviceFetchMode) {
        switch result {
        case .success(let value):
            switch value {
            case .graph(let page):
                graph = .init(key: key, data: page, failed: false)
                graphStats = UsageStats(payload: page.payload, selectedClients: Set(page.payload.summary.clients))
                statsMemo = nil
            case .models(let page):
                models = .init(key: key, data: page, failed: false)
                modelsColors = ModelColorMap(report: page.payload)
            case .hourly(let page): hourly = .init(key: key, data: page, failed: false)
            case .agents(let page): agents = .init(key: key, data: page, failed: false)
            }
        case .failure:
            guard mode == .refresh else { return }
            switch key.kind {
            case .graph: graph = .init(key: key, data: graph?.key == key ? graph?.data : nil, failed: true)
            case .models: models = .init(key: key, data: models?.key == key ? models?.data : nil, failed: true)
            case .hourly: hourly = .init(key: key, data: hourly?.key == key ? hourly?.data : nil, failed: true)
            case .agents: agents = .init(key: key, data: agents?.key == key ? agents?.data : nil, failed: true)
            }
        }
    }

    /// The tab's client set for the overlay page of `kind`: the single
    /// client's slice, else the clients that page itself holds minus the
    /// hidden ones. Deliberately not intersected with this Mac's present
    /// clients: a client only a remote device used belongs on Overview.
    func displayClients(
        _ kind: DeviceOverlayKind, singleClient: String?, hiddenRaw: String, orderRaw: String
    ) -> [String] {
        if let singleClient { return ClientRegistry.tabSlice(singleClient) }
        let present: [String]
        switch kind {
        case .graph: present = graphStats?.presentClients ?? []
        case .models: present = Set(models?.data?.payload.entries.map(\.client) ?? []).sorted()
        case .hourly: present = Set(hourly?.data?.payload.entries.flatMap(\.clients) ?? []).sorted()
        case .agents: present = Set(agents?.data?.payload.entries.flatMap(\.clients) ?? []).sorted()
        }
        return ClientRegistry.displayClients(present: present, hiddenRaw: hiddenRaw, orderRaw: orderRaw)
    }

    /// What a lens may show from `slot`: the page only while the slot is for
    /// the key the lens wants now. After a year or client change the previous
    /// key's page is not shown while the new one loads.
    func visible<P>(
        _ slot: DeviceOverlaySlot<P>?, _ kind: DeviceOverlayKind, year: String?, clients: [String]?
    ) -> (page: DeviceAwarePayload<P>?, failed: Bool) {
        guard let slot, slot.key == key(kind, year: year, clients: clients) else { return (nil, false) }
        return (slot.data, slot.failed)
    }

    /// What the Stats lens' model sections get: the models overlay's report
    /// for the current key, or nothing and a fixed line while it has not
    /// arrived. This Mac's own `modelReport` is never the fallback.
    func statsModels(year: String?) -> (report: ModelReport?, note: String?) {
        if let slot = models, slot.key == key(.models, year: year, clients: nil), let page = slot.data {
            return (page.payload, nil)
        }
        return (nil, DeviceOverlayCopy.modelsHidden.localized)
    }

    /// `UsageStats` for a client slice of the graph overlay (its own memo).
    func stats(selecting selected: Set<String>) -> UsageStats? {
        guard let slot = graph, let full = graphStats, let page = slot.data else { return nil }
        if selected == Set(full.presentClients) { return full }
        if let memo = statsMemo, memo.key == slot.key, memo.selected == selected { return memo.value }
        let computed = UsageStats(payload: page.payload, selectedClients: selected)
        statsMemo = (slot.key, selected, computed)
        return computed
    }

    /// Runs `body` on a `Thread` of its own: a device call can block for a
    /// fetch deadline, and neither the main actor nor the cooperative pool
    /// should wait on it.
    nonisolated private static func onOwnThread<T: Sendable>(
        _ body: @escaping @Sendable () throws -> T
    ) async -> Result<T, Error> {
        await withCheckedContinuation { continuation in
            Thread {
                do { continuation.resume(returning: .success(try body())) } catch {
                    continuation.resume(returning: .failure(error))
                }
            }.start()
        }
    }
}
