import Foundation
import TokenBarCore

/// Pure payload construction for the (opt-in, default-off) Discord Rich
/// Presence feature. Deliberately UI-free and lifecycle-free: `SelfTest.run()`
/// returns `Never` before `NSApplication.shared` exists, so anything the
/// privacy assertions must cover has to be reachable without an `AppDelegate`.
///
/// Nothing here opens a socket, touches the network, reads `UserDefaults`, or
/// performs file I/O — the transport and the opt-in switch land separately.
/// `hidden` is a parameter, not a lookup, for the same reason.
enum DiscordPresence {
    /// Everything about the USER that would be published to a third party and
    /// shown on a public profile — and only that.
    ///
    /// Not the whole profile surface: the transport adds four leaves of its own
    /// (the pid, a nonce, and a repository button's label and URL), all
    /// constants or process facts with nothing of the user in them, declared
    /// and asserted in `DiscordIPC`. An audit needs both halves. This type
    /// bounds what can be said about the person; that file bounds what else can
    /// ride along.
    struct Payload: Equatable {
        let details: String
        let state: String
        let largeImageKey: String

        /// **The published surface derived from the user.** These values, and
        /// only these, carry anything about them: the transport must not drop a
        /// field or alter a value, and no property of this struct that is
        /// absent here is published.
        ///
        /// The transport does add four leaves of its own — the pid, a nonce,
        /// and a repository button's label and URL — and all four are constants
        /// or process facts with nothing of the user in them. They are declared
        /// and asserted in `DiscordIPC`, deliberately not here, because a value
        /// placed in this dictionary is admitted by the wire assertion by
        /// definition. Anything that says something about the user belongs
        /// here, where the scans can reach it.
        ///
        /// Key *naming* is the transport's concern, not this struct's. Discord
        /// nests the asset key as `assets.large_image` rather than carrying it
        /// at the top level, so the transport renames on the way out; that is
        /// not an addition and does not weaken anything above. The wire shape
        /// is deliberately not encoded here — this layer knows what may be
        /// published, not how Discord spells it.
        ///
        /// It is a dictionary rather than a list of values so that one
        /// assertion can pin the key set, and the same assertion is what the
        /// privacy value-scans read. An earlier revision kept a separate
        /// `allFields: [String]` for the scans and reflected over the stored
        /// properties for the key check; that made the published surface and
        /// the asserted surface two different things, and regressions escaped
        /// through the gap in both directions (a computed property is invisible
        /// to `Mirror`; a shrunk list silently narrows every scan). Keep them
        /// one thing.
        /// `state` is omitted when empty rather than published as a blank
        /// string. With one component selected there is nothing to put in it,
        /// and an empty field is still a field: it reaches the wire, and the
        /// key-set assertion would have to admit a key carrying nothing.
        var fields: [String: String] {
            var out = ["details": details, "largeImageKey": largeImageKey]
            if !state.isEmpty { out["state"] = state }
            return out
        }
    }

    /// Shown instead of an unregistered client id.
    static let neutralClientLabel = "an AI tool"

    /// Discord asset key. **Renaming this is an add, never a replace.**
    ///
    /// An asset key cannot be edited once saved in the Developer Portal — the
    /// only way to "rename" one is to delete it and upload again under the new
    /// key. And a key Discord cannot resolve does not error: the image simply
    /// does not appear. So deleting `tokenbar` in favour of a new name would
    /// silently strip the image from the presence of every user still on a
    /// build that asks for the old one, with nothing anywhere reporting it.
    ///
    /// The correct move at a product rename is to upload the new key and keep
    /// this one forever: an application has 300 asset slots and currently uses
    /// one, so carrying both costs nothing and needs no coordination with the
    /// release.
    ///
    /// This warning exists in exactly one place upstream — a line of small
    /// print next to the Portal's upload field — and nowhere in this repo. It
    /// is written here rather than in `docs/` because the declaration is what
    /// the person doing the rename will actually have open.
    static let largeImageKey = "syrtis"

    /// The opt-in switch. Default-off.
    static let enabledKey = "tokenbar.discord.enabled"

    /// The arguments under which this process must never connect, whatever the
    /// preferences say. `--demo` serves fixture numbers
    /// (`UsageDataSources.make`), and `--smoke`/`--selftest` never reach the
    /// app lifecycle at all (`main.swift`).
    /// `--icon-gallery` is in here and `--settings`/`--open-popover` are not,
    /// and the line is drawn at "is this a mode a user runs". The gallery is a
    /// debug window for checking brand art; nobody launches it to use the app,
    /// yet it enters the normal lifecycle and refreshes the live graph, so a
    /// maintainer with the switch already on would publish their real usage
    /// from an asset check. The other two open real UI on real data — that is
    /// a real run of the app and it should behave like one.
    static let testArguments = ["--demo", "--smoke", "--selftest", "--icon-gallery"]

    /// The single authoritative read of the opt-in switch. Everything that
    /// decides whether to connect goes through here; the SwiftUI toggle only
    /// binds `enabledKey`.
    ///
    /// Explicit `object(forKey:) as? Bool`, not `bool(forKey:)`: the latter
    /// coerces a string `"true"` into true, and returns false for a missing key
    /// through the same path it returns false for an explicit off, so it cannot
    /// tell "absent" from "switched off". Publishing to a third party has to
    /// require a real Bool the user actually wrote.
    static func enabled(defaults: UserDefaults = .standard) -> Bool {
        strictBool(enabledKey, defaults: defaults)
    }

    /// The cost-display switch. Default-off means banded, which is the safe
    /// direction: forgetting to write this key cannot make the presence more
    /// revealing than it was.
    static let wholeDollarsKey = "tokenbar.discord.wholeDollars"

    /// The single authoritative read of the cost-display switch, and the only
    /// place a preference decides it. Everything below `payload(...)` takes the
    /// style as a parameter.
    static func costStyle(defaults: UserDefaults = .standard) -> CostStyle {
        strictBool(wholeDollarsKey, defaults: defaults) ? .wholeDollars : .banded
    }

    /// Opt-in: also count the other devices selected in the dashboard. Default
    /// off, strict Bool, same reader as the other two switches.
    static let includeDevicesKey = "tokenbar.discord.includeDevices"

    static func includeDevices(defaults: UserDefaults = .standard) -> Bool {
        strictBool(includeDevicesKey, defaults: defaults)
    }

    /// The peers the dashboard has selected (`tokenbar.dashboard.deviceSelection`,
    /// JSON `{local, peers}`; the key is `DeviceOverlayState.selectionKey`):
    /// deduplicated, sorted, at most the core's limit of 16. Missing or
    /// undecodable means none. `local` is ignored on purpose: Discord always
    /// counts this Mac.
    /// Copies of `DeviceOverlayState.selectionKey` and `.maxSelectedPeers`
    /// (main-actor isolated there, and this is nonisolated); a selftest pins
    /// that they are equal.
    static let dashboardSelectionKey = "tokenbar.dashboard.deviceSelection"
    static let maxSelectedPeers = 16

    static func selectedPeers(defaults: UserDefaults = .standard) -> [String] {
        guard let data = defaults.data(forKey: dashboardSelectionKey),
              let stored = try? JSONDecoder().decode(StoredDeviceSelection.self, from: data)
        else { return [] }
        return Array(Set(stored.peers).sorted().prefix(maxSelectedPeers))
    }

    /// Which graph Discord is built from, and when to ask the core for a merged
    /// one. Pure: `AppDelegate` calls it and carries out its decision.
    ///
    /// Merging this Mac with other devices happens ONLY in the core
    /// (`UsageDataSource.graphDevices`); nothing here adds or splices numbers.
    /// The tray title, client trays and `lastGraph` never see `merged`.
    enum DeviceMerge {
        /// Same value as `DeviceOverlayState.remotePollSeconds`, for the same
        /// reason: a producer grants a burst of 2 requests per grant and refills
        /// one per 60 s.
        static let refreshSeconds: TimeInterval = 300
        /// A held result survives this many failed calls in a row, then goes:
        /// one failure should not make the published figure dip to this Mac's
        /// alone, but a result nothing can renew stops counting this Mac's new
        /// usage and has no entry for today after midnight. Calls come one per
        /// tray poll (5 min closed, 60 s open), so this is at most 15 min.
        static let maxFailuresHeld = 3

        struct State {
            /// The merged `UsagePayload` only. The per-device rows are dropped:
            /// their label and address are the other owner's free text.
            var merged: UsagePayload?
            /// The peers `merged` was fetched for.
            var mergedPeers: [String] = []
            var generation: UInt64 = 0
            var lastRefresh: Date?
            var inFlight = false
            /// Calls that failed in a row since `merged` was last stored.
            var failures = 0
            // The inputs last seen, so any change bumps `generation`.
            var includeDevices = false
            var discordOn = false
            var peers: [String] = []
        }

        struct Call: Equatable {
            let mode: DeviceFetchMode
            /// Always `local: true`, whatever the dashboard's own tick says.
            let selection: DeviceSelection
            let generation: UInt64
        }

        struct Decision {
            /// What Discord is built from this moment (never the tray's input).
            let graph: UsagePayload?
            let call: Call?
            let state: State
        }

        /// Merge is wanted: switched on, this process may connect (the
        /// `makeDiscordClient` gate) and at least one other device is selected.
        static func active(includeDevices: Bool, canConnect: Bool, peers: [String]) -> Bool {
            includeDevices && canConnect && !peers.isEmpty
        }

        /// The inputs moved: a new generation, and the old result goes now.
        static func observe(
            _ state: State, includeDevices: Bool, discordOn: Bool, peers: [String]
        ) -> State {
            var next = state
            if includeDevices != state.includeDevices || discordOn != state.discordOn
                || peers != state.peers {
                next.generation &+= 1
                next.merged = nil
                next.mergedPeers = []
            }
            next.includeDevices = includeDevices
            next.discordOn = discordOn
            next.peers = peers
            return next
        }

        /// A linked device was removed or re-paired: drop the result at once.
        static func invalidate(_ state: State) -> State {
            var next = state
            next.generation &+= 1
            next.merged = nil
            next.mergedPeers = []
            return next
        }

        /// The graph to publish from, re-checked against the conditions of
        /// the moment of publishing.
        static func graph(
            local: UsagePayload?, state: State, includeDevices: Bool, canConnect: Bool,
            peers: [String]
        ) -> UsagePayload? {
            guard active(includeDevices: includeDevices, canConnect: canConnect, peers: peers),
                  let merged = state.merged, state.mergedPeers == peers
            else { return local }
            return merged
        }

        static func decide(
            local: UsagePayload?, state: State, includeDevices: Bool, discordOn: Bool,
            canConnect: Bool, peers: [String], panelOpen: Bool, now: Date
        ) -> Decision {
            var next = observe(
                state, includeDevices: includeDevices, discordOn: discordOn, peers: peers)
            let wanted = active(includeDevices: includeDevices, canConnect: canConnect, peers: peers)
            if !wanted { next.merged = nil; next.mergedPeers = [] }
            let used = graph(
                local: local, state: next, includeDevices: includeDevices,
                canConnect: canConnect, peers: peers)
            // One call at a time; a tick that finds one running is skipped.
            guard wanted, !next.inFlight else {
                return Decision(graph: used, call: nil, state: next)
            }
            // `refresh` pulls every device and blocks: only while the panel is
            // closed (the dashboard keeps last-good fresh while it is open) and
            // only every `refreshSeconds`. Otherwise `cached`: this Mac
            // recomputed, each device's last good, no connection.
            let due = next.lastRefresh.map { now.timeIntervalSince($0) >= refreshSeconds } ?? true
            let mode: DeviceFetchMode = (!panelOpen && due) ? .refresh : .cached
            if mode == .refresh { next.lastRefresh = now }
            next.inFlight = true
            return Decision(
                graph: used,
                call: Call(
                    mode: mode, selection: DeviceSelection(local: true, peers: peers),
                    generation: next.generation),
                state: next)
        }

        /// A call came back. A result from an older generation is dropped; a
        /// failure keeps what was there until `maxFailuresHeld` in a row.
        static func arrived(_ state: State, call: Call, payload: UsagePayload?) -> State {
            var next = state
            next.inFlight = false
            guard call.generation == state.generation else { return next }
            if let payload {
                next.merged = payload
                next.mergedPeers = call.selection.peers
                next.failures = 0
            } else {
                next.failures += 1
                if next.failures >= maxFailuresHeld {
                    next.merged = nil
                    next.mergedPeers = []
                }
            }
            return next
        }

        /// Which direction did the set of other devices on the profile move.
        /// Removing one retires earlier queued work (it counted a device that
        /// is no longer wanted); adding one does not.
        static func visibility(
            previous: (active: Bool, peers: [String]), current: (active: Bool, peers: [String])
        ) -> DiscordIPC.VisibilityChange {
            let before = previous.active ? Set(previous.peers) : []
            let after = current.active ? Set(current.peers) : []
            return before.isSubset(of: after) ? .none : .reducing
        }
    }

    /// What the presence may be built from. The case ORDER is the published
    /// order: the first selected component becomes `details` and the rest join
    /// into `state`, so the default set reproduces exactly what was published
    /// before composition existed.
    enum Component: String, CaseIterable {
        case tokens
        case client
        case cost
    }

    /// Which client's usage is published: the busiest one, or a named agent.
    enum ClientSelection: Equatable {
        case mostUsed
        case only(String)
        /// The key is present but holds something that is not a string — a
        /// malformed `defaults write`. Publishes nothing, the same answer an
        /// unknown id gets, rather than widening to every registered client.
        case malformed
    }

    static let selectionKey = "tokenbar.discord.client"

    /// Matches no radio option, so a malformed stored value ticks nothing
    /// rather than claiming a selection the payload path does not agree with.
    static let malformedSelectionLabel = "\u{0}malformed"

    /// Absent or empty means the busiest visible client, which is what the
    /// feature published before this preference existed.
    /// Absent, malformed and named are three different answers, for the same
    /// reason `components()` distinguishes them: one `as? String` cast would
    /// send a key holding a number down the ABSENT branch and silently widen a
    /// one-client selection to every registered client, while an unknown
    /// string id correctly publishes nothing.
    static func selection(defaults: UserDefaults = .standard) -> ClientSelection {
        guard let stored = defaults.object(forKey: selectionKey) else { return .mostUsed }
        guard let id = stored as? String else { return .malformed }
        return id.isEmpty ? .mostUsed : .only(id)
    }

    static let componentsKey = "tokenbar.discord.components"

    /// Absent means all three. Unlike the two switches the safe direction here
    /// is not "off": an absent key must not silently empty a presence the user
    /// already consented to.
    static let defaultComponents = Set(Component.allCases)

    static func components(defaults: UserDefaults = .standard) -> Set<Component> {
        // Absent and malformed are different answers on purpose. An ABSENT key
        // is an upgrade from before this preference existed, and must not
        // silently empty a presence the user already consented to. A key that
        // is PRESENT but not a string is a malformed write, and gets the same
        // answer a string of only unknown tokens gets: nothing. Collapsing the
        // two would mean `defaults write ... -int 1` publishes all three
        // components the user never selected, which is the wrong direction for
        // the same input class.
        guard let stored = defaults.object(forKey: componentsKey) else {
            return defaultComponents
        }
        guard let raw = stored as? String else { return [] }
        return parseComponents(raw)
    }

    /// A fixed allowlist by construction: `Component(rawValue:)` returns nil for
    /// anything it does not know and `compactMap` drops it.
    ///
    /// That is the point. This preference is a user-controlled string flowing
    /// toward a public profile — the same shape as the `cc-mirror/<name>` client
    /// id that could once escape as a label. An unknown token must produce
    /// *nothing*: never echoed, and with no fallback branch passing it through.
    /// A wrong value here empties the presence, which is the harmless direction.
    ///
    /// Shared with the Settings checkboxes so the view and the payload cannot
    /// drift into two readings of one string.
    static func parseComponents(_ raw: String) -> Set<Component> {
        Set(
            raw.split(separator: ",")
                .compactMap { Component(rawValue: $0.trimmingCharacters(in: .whitespaces)) })
    }

    /// Canonical form: `Component.allCases` order, no spaces. Writing it this
    /// way means a reordering is never stored, so the value gate cannot mistake
    /// one for a change in what is published.
    static func rawComponents(_ components: Set<Component>) -> String {
        Component.allCases.filter(components.contains).map(\.rawValue).joined(separator: ",")
    }

    static let defaultComponentsRaw = rawComponents(Set(Component.allCases))

    /// The agents the picker may offer, in the user's own tab order.
    ///
    /// Derived from the two conditions `payload` already enforces, rather than
    /// from the registry: a row is only publishable when the id is registered
    /// (an unregistered one returns nil at the `.only` guard) and not hidden
    /// (`trayTotals` filters it out of the totals). Offering anything else is a
    /// row that reads as a choice and silently publishes nothing — the failure
    /// the whole feature is built to avoid, arriving through the picker.
    /// `present` is nil until an accepted payload lands, and DISTINCT from an
    /// empty one: Settings can be opened before the first scan, where an empty
    /// picker would read as "your agents are gone". A loaded-but-empty scan is
    /// a real answer — no usage at all, or every used client hidden — and the
    /// honest picker there offers only "whichever client you used most".
    /// Collapsing the two would repopulate the list with the whole registry for
    /// a user who has no usage, which is the defect this exists to remove.
    ///
    /// nil also covers a graph fetch that keeps failing, since that never
    /// assigns a payload. The registry list is the right answer there too: with
    /// no data at all, nothing is knowable about which agents are in use, and
    /// this is what the picker showed before the filter existed.
    static func selectableClients(
        present: [String]?, hiddenRaw: String, orderRaw: String, selection: ClientSelection
    ) -> [String] {
        let registered = Set(ClientRegistry.allIds)
        guard let present else {
            return ClientRegistry.orderedClients(
                ClientRegistry.allIds.filter {
                    !ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw)).contains($0)
                }, orderRaw: orderRaw)
        }
        var out = ClientRegistry
            .displayClients(present: present, hiddenRaw: hiddenRaw, orderRaw: orderRaw)
            .filter(registered.contains)
        // A stored selection that stopped qualifying — the user hid it, or that
        // agent has no usage — stays listed. Dropping it would tick nothing
        // while the preference still names it, showing a state the app is not
        // in. Only while it stays REGISTERED: an id the registry no longer
        // knows is one `payload` rejects outright, so listing it would tick a
        // row that can never publish. Nothing ticked is the honest answer
        // there, and the same one `.malformed` already gets.
        if case .only(let id) = selection, registered.contains(id), !out.contains(id) {
            out.append(id)
        }
        return out
    }

    /// One reader for both switches, so their strictness cannot drift apart.
    ///
    /// `as? Bool` alone is not enough: `NSNumber` bridges, so an integer 1 or a
    /// double 1.0 sitting in one of these keys would read as "on". Nothing this
    /// app writes produces that, but "an explicit Bool the user wrote" is the
    /// contract, and a type check that accepts three other types is not that
    /// contract. Only a real CFBoolean counts.
    ///
    /// `bool(forKey:)` is wrong for a different reason: it coerces the string
    /// `"true"`, and it returns false for a missing key through the same path
    /// it returns false for an explicit off, so it cannot tell absent from
    /// switched-off.
    private static func strictBool(_ key: String, defaults: UserDefaults) -> Bool {
        guard let stored = defaults.object(forKey: key) as? NSNumber,
              CFGetTypeID(stored) == CFBooleanGetTypeID()
        else { return false }
        return stored.boolValue
    }

    /// The one place that decides whether this process may ever connect.
    ///
    /// Test flags outrank the preference, and that order is the whole point:
    /// `docs/knowledge/verification.md`'s manual flow sets preferences straight
    /// from the command line (`-tokenbar.<key> <value>`) on the same machine and
    /// the same defaults domain as the user's real app, so a demo run whose
    /// defaults say "on" must still stay silent. Fixture numbers on a real
    /// Discord profile is the one failure this feature cannot take back.
    static func mayConnect(arguments: [String], enabled: Bool) -> Bool {
        guard !arguments.contains(where: testArguments.contains) else { return false }
        return enabled
    }

    /// Fixed allowlist: a registered id gets its registry display name, anything
    /// else gets the neutral constant.
    ///
    /// The gate is the point. A graph client id can be `cc-mirror/<name>` where
    /// `<name>` comes straight out of a user's local config file and is only
    /// character-filtered, never semantically constrained
    /// (vendor/tokscale-core/src/sessions/claudecode.rs:60-62). Falling through
    /// to `ClientRegistry.style(id)` would title-case that string and publish an
    /// employer or internal-tool name on a public Discord profile. Gating first
    /// means `style()`'s title-case fallback branch is unreachable from here.
    static func safeClientLabel(_ id: String?) -> String {
        guard let id, ClientRegistry.allIds.contains(id) else { return neutralClientLabel }
        return ClientRegistry.style(id).displayName
    }

    /// How a cost is rendered. A parameter threaded through `payload(...)` and
    /// never a preference lookup: this layer performs none, and the reason is
    /// operational rather than stylistic. `verification.md`'s manual flow writes
    /// preferences from the command line into the same defaults domain the
    /// selftest runs in, so a payload path that read one would make the privacy
    /// assertions depend on the state of the machine running them.
    ///
    /// There is deliberately no default value on the parameter either. A
    /// default is how "the implementation ignores the setting" compiles.
    enum CostStyle: Equatable {
        case banded
        case wholeDollars
    }

    /// Coarse cost band rather than `$%.2f`: the cost ÷ tokens ratio leaks the
    /// model tier and cache structure in use, and accumulated daily costs leak a
    /// monthly spend bracket.
    ///
    /// The shape is logarithmic with an open tail, and both halves of that are
    /// load-bearing. Coarse at the bottom because most days live there and the
    /// old `<$1`/`$1-5`/`$5-10` split published three bits about the median
    /// user for nothing. Open at the top because any finite top band publishes
    /// more than today's `$100+` did precisely where the anonymity set is
    /// smallest — a daily figure near $1,900 is a monthly spend near $50k, and
    /// very few individual profiles sit there.
    ///
    /// "Logarithmic" describes the table, not the arithmetic. There is no
    /// `log10` here on purpose: it would map zero to `-inf` and a negative to
    /// `NaN`, both of which match no `case` and land on the top band. Literal
    /// bounds let negatives and zero fall into the lowest band by ordinary
    /// comparison, which is what a negative daily total should read as. They
    /// do occur — `trayTotals`' slow path cannot reproduce the day-level
    /// `.max(0)`, see its doc comment.
    ///
    /// Bounds are half-open and lower-inclusive: $50 is `$50-100`, not
    /// `$10-50`.
    static func costBucket(_ cost: Double) -> String {
        // Before the comparisons, not after. A NaN matches no range and would
        // reach the `default` arm, publishing the TOP band for a value that
        // means nothing at all. `payload(...)` rejects a non-finite cost before
        // it ever gets here and remains the primary defence; this is the second
        // one, for the day someone adds a call site.
        guard cost.isFinite else { return "<$10" }
        switch cost {
        case ..<10: return "<$10"
        case ..<50: return "$10-50"
        case ..<100: return "$50-100"
        case ..<250: return "$100-250"
        case ..<500: return "$250-500"
        case ..<1000: return "$500-1000"
        default: return "$1000+"
        }
    }

    /// Opt-in whole dollars. Never cents: a daily figure carrying a fractional
    /// part is a near-unique fingerprint across a month, so anyone holding a
    /// second usage record — a bill, a shared dashboard, a screenshot — can
    /// match a public profile to an account. Rounding to the dollar removes
    /// that entropy while still showing a real number.
    ///
    /// Total by construction, which is the whole reason it is written this way
    /// rather than as `"$" + Int(max(0, cost).rounded())`. That expression
    /// looks like it renders infinity as `$0` and does not: `max` folds only
    /// `NaN` and `-.infinity` to the other operand, so `max(0, .infinity)` is
    /// `.infinity` and `Int(.infinity)` **traps**. So does `Int(1e308)`, and
    /// `1e308` is finite, so it clears `payload(...)`'s `isFinite` guard and
    /// arrives here. A trap is worse than any wrong band: it aborts the
    /// process, and in the selftest it produces no FAIL line and no verdict —
    /// the run simply disappears.
    ///
    /// The million-dollar cap is a safety bound, not a privacy one. It also
    /// keeps the string short, which matters because Discord's `details` and
    /// `state` have length limits.
    static func wholeDollars(_ cost: Double) -> String {
        guard cost.isFinite, cost > 0 else { return "$0" }
        // Rounded BEFORE the cap is judged, not after. Comparing the raw cost
        // put `[999999.5, 1_000_000)` on the wrong side of it: those round to
        // 1000000 and were rendered as `$1000000`, a figure the cap exists to
        // avoid printing bare. One value, two spellings, for no reason.
        let dollars = cost.rounded()
        guard dollars < 1_000_000 else { return "$1000000+" }
        return "$\(Int(dollars))"
    }

    static func costText(_ cost: Double, style: CostStyle) -> String {
        switch style {
        case .banded: return costBucket(cost)
        case .wholeDollars: return wholeDollars(cost)
        }
    }

    /// Coarse token band. `Format.compactTokens` is the shared tray-title
    /// formatter and returns the exact `Int64` below 1000 (`Format.swift:19`),
    /// which is correct in the menu bar and wrong here: the published figure
    /// must never be an exact count. A negative total lands here too — the
    /// aggregator's per-lane clamping admits pathological negative deltas
    /// (see `trayTotals`' doc comment), and `String(count)` would publish the
    /// full signed digits.
    ///
    /// Do not "fix" this by changing `Format.compactTokens`; the tray title
    /// wants the exact small number.
    static func tokenBand(_ tokens: Int64) -> String {
        tokens < 1_000 ? "<1K" : Format.compactTokens(tokens)
    }

    /// The publishable payload for `today`, or nil when nothing may be
    /// published. `hidden` must be the tab-hidden set (`ClientRegistry
    /// .hiddenClients()` at the call site) — `trayTotals` applies it to the same
    /// stripes the top client is folded from.
    static func payload(
        graph: UsagePayload, hidden: Set<String>, today: String, costStyle: CostStyle,
        components: Set<Component>, selection: ClientSelection = .mostUsed
    ) -> Payload? {
        // Derived HERE, not in the wiring. Putting it in `AppDelegate` would
        // repeat the earlier breach word for word: logic the payload fixtures
        // cannot see, guarded only by a source scan that keeps passing while
        // the derivation underneath it changes.
        //
        // A positive allowlist in BOTH modes, which is what makes
        // `safeClientLabel`'s neutral branch unreachable from here. An
        // unregistered id — `cc-mirror/<user-chosen name>`, or any agent the
        // aggregator detects before the registry catches up — no longer
        // publishes under a neutral label; it contributes nothing at all.
        //
        // The consequence is deliberate and belongs in the Settings copy, not
        // in a bug report: the presence total can differ from the tray total
        // whenever an unregistered client has usage.
        let only: Set<String>
        switch selection {
        case .mostUsed:
            only = Set(ClientRegistry.allIds)
        case .only(let id):
            // Not a fallback to most-used. A selection the registry no longer
            // knows — a rename, a typo written from the command line — would
            // otherwise silently widen "one agent" to "all of them", which is
            // the wrong direction; and if it matched an unregistered id it
            // would target exactly that client's figures under a neutral label.
            guard ClientRegistry.allIds.contains(id) else { return nil }
            only = [id]
        case .malformed:
            return nil
        }
        let totals = graph.trayTotals(hidden: hidden, today: today, only: only)
        // Non-finite numbers must never be published — garbage on a public
        // profile is worse than no presence at all. Scoped to a cost that will
        // actually be serialized: `costBucket` maps a non-finite value to the
        // LOWEST band, so publishing it would assert something false, but a
        // user who selected only tokens loses their whole presence to an
        // overflow that was never going to reach the wire.
        guard components.contains(.cost) ? totals.todayCost.isFinite : true else { return nil }
        // Zero usage would otherwise publish a "this machine is switched on"
        // beacon. Nothing is published, and no startTimestamp is ever emitted.
        // `||`, not `&&`: UsageStats allows a day with cost > 0 and tokens == 0.
        guard totals.todayTokens > 0 || totals.todayCost > 0 else { return nil }
        // Built in `Component.allCases` order, so the published text does not
        // depend on the order the user ticked the boxes, and the default set
        // reproduces the pre-composition output exactly.
        let parts = Component.allCases.filter(components.contains).map { component -> String in
            switch component {
            case .tokens: return "\(tokenBand(totals.todayTokens)) tokens today"
            case .client: return safeClientLabel(totals.todayTopClient)
            case .cost: return costText(totals.todayCost, style: costStyle)
            }
        }
        // An empty composition publishes nothing at all, and this is where that
        // is enforced rather than in the picker — the manual verification flow
        // writes preferences straight from the command line into the same
        // defaults domain, so a guard living in the picker would not be in the
        // path. An activity with no components would still carry the app name,
        // the image and the button and still refresh on the same cadence: a
        // purer version of the "this machine is on" beacon the zero-usage rule
        // exists to prevent, with no usage content to justify it.
        //
        // No separate `components.isEmpty` guard above: it would be dead code.
        // Measured — removing one changed nothing, because an empty selection
        // produces no parts and this is the line that stops it.
        guard let details = parts.first else { return nil }
        return Payload(
            details: details,
            state: parts.dropFirst().joined(separator: " · "),
            largeImageKey: largeImageKey)
    }
}
