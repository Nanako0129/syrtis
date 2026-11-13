import AppKit
import Darwin
import Foundation
import TokenBarCore

// The Settings "Devices" page (SA-3c): sharing this Mac, the scan-scope
// consent and pairing. Every decision about sharing, consent, pairing and
// grants is made by the embedded agent (`remote_agent.rs`, Syrtis-Agent);
// this model only calls the `TBCore.remote*` wrappers, keeps what they
// answer and maps their fixed error classes to copy. It computes no digest
// and authorizes nothing.

/// The `TBCore.remote*` calls the page makes. Blocking; the model calls them
/// off the main thread. Injected so the selftest drives the model with a fake.
protocol DevicesRemoteClient: Sendable {
    func start() throws -> RemoteAgentStatus
    func status() throws -> RemoteAgentStatus
    func setSharing(_ enabled: Bool) throws
    func listenConfigure(host: String, port: Int, nodeLabel: String?) throws -> RemoteAgentStatus
    func scopeDisclosure() throws -> RemoteScopeDisclosure
    func scopeApprove(ownerInputsSha256: String) throws
    func offerCreate(ttlSeconds: Int64) throws -> RemoteOffer
    func offerCancel(offerId: String) throws
    func pendingList() throws -> [RemotePending]
    func approve(pendingId: String) throws
    func reject(pendingId: String) throws
    func grants() throws -> [RemoteGrant]
    func revoke(grantId: String) throws
    func pairClaim(pairUrl: String) throws -> RemoteClaim
    /// Blocks up to the frozen 960 s poll window; no cancellation in the ABI.
    func pairPoll(handle: Int64) throws -> RemotePaired
    func peers() throws -> [RemotePeer]
    func peerRemove(producerStatic: String) throws
}

struct LiveDevicesClient: DevicesRemoteClient {
    /// The app's own start path and root (`RemoteAgentLaunch`).
    func start() throws -> RemoteAgentStatus {
        try RemoteAgentLaunch.start(home: RemoteAgentLaunch.homeOverride())
    }
    func status() throws -> RemoteAgentStatus { try TBCore.remoteStatus() }
    func setSharing(_ enabled: Bool) throws { try TBCore.remoteSetSharing(enabled) }
    func listenConfigure(host: String, port: Int, nodeLabel: String?) throws -> RemoteAgentStatus {
        try TBCore.remoteListenConfigure(host: host, port: port, nodeLabel: nodeLabel)
    }
    func scopeDisclosure() throws -> RemoteScopeDisclosure { try TBCore.remoteScopeDisclosure() }
    func scopeApprove(ownerInputsSha256: String) throws {
        try TBCore.remoteScopeApprove(ownerInputsSha256: ownerInputsSha256)
    }
    func offerCreate(ttlSeconds: Int64) throws -> RemoteOffer {
        try TBCore.remoteOfferCreate(ttlSeconds: ttlSeconds)
    }
    func offerCancel(offerId: String) throws { try TBCore.remoteOfferCancel(offerId: offerId) }
    func pendingList() throws -> [RemotePending] { try TBCore.remotePendingList() }
    func approve(pendingId: String) throws { try TBCore.remoteApprove(pendingId: pendingId, sasGeneration: 1) }
    func reject(pendingId: String) throws { try TBCore.remoteReject(pendingId: pendingId, sasGeneration: 1) }
    func grants() throws -> [RemoteGrant] { try TBCore.remoteGrants() }
    func revoke(grantId: String) throws { try TBCore.remoteRevoke(grantId: grantId) }
    func pairClaim(pairUrl: String) throws -> RemoteClaim { try TBCore.remotePairClaim(pairUrl: pairUrl) }
    func pairPoll(handle: Int64) throws -> RemotePaired { try TBCore.remotePairPoll(handle: handle) }
    func peers() throws -> [RemotePeer] { try TBCore.remotePeers() }
    func peerRemove(producerStatic: String) throws {
        try TBCore.remotePeerRemove(producerStatic: producerStatic)
    }
}

/// Where the Pair URL is copied to. Injected so the selftest never touches
/// the real pasteboard.
@MainActor
protocol DevicesPasteboard: AnyObject {
    var changeCount: Int { get }
    /// Writes `text` marked concealed and transient; returns the change count
    /// right after the write.
    func writeSecret(_ text: String) -> Int
    func clear()
}

@MainActor
final class LiveDevicesPasteboard: DevicesPasteboard {
    /// The nspasteboard.org markers: clipboard managers and Universal
    /// Clipboard skip concealed and transient items.
    private static let concealed = NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")
    private static let transient = NSPasteboard.PasteboardType("org.nspasteboard.TransientType")

    var changeCount: Int { NSPasteboard.general.changeCount }

    func writeSecret(_ text: String) -> Int {
        let board = NSPasteboard.general
        board.clearContents()
        let item = NSPasteboardItem()
        item.setString(text, forType: .string)
        item.setData(Data(), forType: Self.concealed)
        item.setData(Data(), forType: Self.transient)
        board.writeObjects([item])
        return board.changeCount
    }

    func clear() { NSPasteboard.general.clearContents() }
}

/// The listen-address menu. Only this Mac's own non-loopback addresses and
/// "this Mac only"; never an unspecified address (`0.0.0.0`, `::`).
enum ListenAddress {
    static let localOnly = "127.0.0.1"

    /// `localOnly` first, then each usable address once, in interface order,
    /// then each of `keeping` (the configured address, the current selection)
    /// that is not listed yet. Never an unspecified or loopback address, from
    /// either source.
    ///
    /// Offered from the interfaces: IPv4, and IPv6 unique-local addresses
    /// (`fc00::/7`: Tailscale, other overlays, a LAN's own prefix). Global
    /// IPv6 is not offered: a Mac carries many such addresses, most of them
    /// privacy addresses that rotate, and this list cannot tell those from a
    /// stable one without each address's interface flags. A global address
    /// that is already configured stays through `keeping`.
    /// ponytail: no way to pick a new global IPv6 address; read
    /// `IN6_IFF_TEMPORARY` per address if someone needs it.
    static func choices(from addresses: [String], keeping: [String] = []) -> [String] {
        var seen: Set<String> = [localOnly]
        var out = [localOnly]
        func usable(_ address: String) -> [UInt8]? {
            guard !address.contains("%"), let bytes = bytes(address),
                  !isUnspecified(bytes), !isLoopback(bytes)
            else { return nil }
            return bytes
        }
        for address in addresses {
            guard let bytes = usable(address), bytes.count == 4 || isUniqueLocalV6(bytes),
                  seen.insert(address).inserted
            else { continue }
            out.append(address)
        }
        for address in keeping where usable(address) != nil && seen.insert(address).inserted {
            out.append(address)
        }
        return out
    }

    /// IPv6 unique-local, `fc00::/7`.
    private static func isUniqueLocalV6(_ bytes: [UInt8]) -> Bool {
        bytes.count == 16 && bytes[0] & 0xFE == 0xFC
    }

    /// A public IPv4 address or a global IPv6 one (2000::/3): it may be
    /// reachable from the internet.
    static func mayBePublic(_ address: String) -> Bool {
        guard let bytes = bytes(address) else { return false }
        if bytes.count == 16 { return bytes[0] & 0xE0 == 0x20 }
        let (a, b) = (bytes[0], bytes[1])
        let local = a == 10 || a == 127 || (a == 172 && (16...31).contains(b))
            || (a == 192 && b == 168) || (a == 169 && b == 254)
            || (a == 100 && (64...127).contains(b))  // carrier-grade NAT (RFC 6598)
        return !local && !isUnspecified(bytes)
    }

    /// This Mac's addresses (`getifaddrs`): interfaces that are up, not
    /// loopback, IPv4 or IPv6, as numeric text.
    nonisolated static func interfaceAddresses() -> [String] {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return [] }
        defer { freeifaddrs(head) }
        var out: [String] = []
        for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let flags = Int32(entry.pointee.ifa_flags)
            guard flags & IFF_UP != 0, flags & IFF_LOOPBACK == 0,
                  let address = entry.pointee.ifa_addr
            else { continue }
            let family = Int32(address.pointee.sa_family)
            let length: Int
            switch family {
            case AF_INET: length = MemoryLayout<sockaddr_in>.size
            case AF_INET6: length = MemoryLayout<sockaddr_in6>.size
            default: continue
            }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(
                address, socklen_t(length), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0
            else { continue }
            out.append(String(decoding: host.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self))
        }
        return out
    }

    private static func bytes(_ address: String) -> [UInt8]? {
        var v4 = in_addr()
        if inet_pton(AF_INET, address, &v4) == 1 {
            return withUnsafeBytes(of: &v4) { Array($0) }
        }
        var v6 = in6_addr()
        if inet_pton(AF_INET6, address, &v6) == 1 {
            return withUnsafeBytes(of: &v6) { Array($0) }
        }
        return nil
    }

    private static func isUnspecified(_ bytes: [UInt8]) -> Bool { bytes.allSatisfy { $0 == 0 } }

    private static func isLoopback(_ bytes: [UInt8]) -> Bool {
        bytes.count == 4 ? bytes[0] == 127 : bytes == [UInt8](repeating: 0, count: 15) + [1]
    }

}

@MainActor
final class DevicesModel: ObservableObject, CustomStringConvertible, CustomDebugStringConvertible {
    /// Process-wide, like `ClaudeRootsListModel.shared`: `SettingsPanel`
    /// rebuilds the page on every switch, so a model owned by the view would
    /// lose a pairing wait.
    static let shared = DevicesModel(client: LiveDevicesClient(), pasteboard: LiveDevicesPasteboard())

    /// How often the page re-reads status, pending requests, grants and
    /// peers while it is shown: a claim's SAS row should appear within a
    /// couple of seconds of the other device pasting the invitation.
    static let refreshInterval: Duration = .seconds(2)
    /// Invitation lifetime. The agent takes 1–900 s; 600 is its CLI default.
    static let offerTTLSeconds: Int64 = 600
    /// The longest Pair URL the field accepts (the contract's bound).
    static let pairUrlMaxLength = 16_384
    /// The agent's node-label bound (`NODE_LABEL_MAX_BYTES`).
    static let nodeLabelMaxBytes = 128
    static let portRange = 1024...65535

    enum Pairing: Equatable {
        case idle
        case claiming
        case waiting(RemoteClaim)
        case linked(fingerprint: String)
        /// A `DevicesCopy` key.
        case failed(String)
    }

    @Published private(set) var status: RemoteAgentStatus?
    /// `DevicesCopy` keys, nil when there is nothing to say.
    @Published private(set) var startFailure: String?
    @Published private(set) var sharingError: String?
    @Published private(set) var listenError: String?
    @Published private(set) var scopeNotice: String?
    @Published private(set) var offerError: String?
    @Published private(set) var decideError: String?
    @Published private(set) var manageError: String?
    @Published private(set) var disclosure: RemoteScopeDisclosure?
    /// The only place the Pair URL lives. Cleared when the page disappears
    /// and when the invitation expires, together with `remoteOfferCancel`.
    @Published private(set) var offer: RemoteOffer?
    @Published private(set) var copied = false
    @Published private(set) var pending: [RemotePending] = []
    @Published private(set) var grants: [RemoteGrant] = []
    @Published private(set) var peers: [RemotePeer] = []
    @Published private(set) var pairing: Pairing = .idle
    /// Pending ids with an Approve or Reject in flight.
    @Published private(set) var deciding: Set<String> = []
    @Published private(set) var working = false

    private let client: DevicesRemoteClient
    private let pasteboard: DevicesPasteboard
    /// The pasteboard change count right after the Pair URL was copied; the
    /// clear empties the pasteboard only if nothing was copied since.
    private var copiedChangeCount: Int?
    /// Claims whose wait the user cancelled; a later success is removed.
    private var cancelledHandles: Set<Int64> = []
    private var refreshTask: Task<Void, Never>?
    /// Bumped by every create and by `pageDisappeared`: a create publishes
    /// only if it is still the latest.
    private var offerGeneration = 0

    init(client: DevicesRemoteClient, pasteboard: DevicesPasteboard) {
        self.client = client
        self.pasteboard = pasteboard
    }

    // Never the Pair URL: string interpolation and logging print this.
    nonisolated var description: String { "DevicesModel" }
    nonisolated var debugDescription: String { description }

    var running: Bool { status?.running == true }
    var sharingOn: Bool { status?.sharingEnabled == true }
    var scopeApproved: Bool { status?.scopeApproved == true }
    /// On only once the scope is approved; off always.
    var canToggleSharing: Bool { running && (sharingOn || scopeApproved) }
    /// No address, or one set that does not listen.
    var unreachable: Bool { status?.bindHost == nil || status?.listening != true }
    var showsRetry: Bool { !running }

    // MARK: Page lifetime

    func pageAppeared() {
        refreshTask?.cancel()
        refreshTask = Task { [weak self] in
            await self?.loadDisclosure()
            while !Task.isCancelled {
                await self?.refresh()
                try? await Task.sleep(for: Self.refreshInterval)
            }
        }
    }

    /// Stops the refresh and clears the invitation. A pairing wait goes on:
    /// its result is shown when the page comes back.
    @discardableResult
    func pageDisappeared() -> Task<Void, Never>? {
        refreshTask?.cancel()
        refreshTask = nil
        // A create still in flight must not publish its offer afterwards.
        offerGeneration &+= 1
        return clearOffer()
    }

    // MARK: Status

    /// Every await is followed by a cancellation check: when the page goes
    /// away mid-refresh (`pageDisappeared` cancels the polling task), the
    /// rest of that refresh makes no call and writes nothing.
    func refresh() async {
        expireOfferIfDue(nowMs: Int64(Date().timeIntervalSince1970 * 1000))
        let result = await call({ try $0.status() })
        guard !Task.isCancelled else { return }
        switch result {
        case .success(let next):
            let wasApproved = status?.scopeApproved == true
            status = next
            guard next.running else {
                pending = []
                grants = []
                peers = []
                return
            }
            startFailure = nil
            if disclosure == nil {
                await loadDisclosure()
            } else if wasApproved, next.scopeApproved != true {
                // The agent restarted over changed inputs: show the new
                // scope and say so, rather than keep the old one on screen.
                scopeNotice = DevicesCopy.scopeChanged
                await loadDisclosure()
            }
            guard !Task.isCancelled else { return }
            let pendingResult = await call({ try $0.pendingList() })
            guard !Task.isCancelled else { return }
            if case .success(let list) = pendingResult { pending = list }
            let grantsResult = await call({ try $0.grants() })
            guard !Task.isCancelled else { return }
            if case .success(let list) = grantsResult { grants = list }
            let peersResult = await call({ try $0.peers() })
            guard !Task.isCancelled else { return }
            if case .success(let list) = peersResult { peers = list }
        case .failure:
            status = nil
        }
    }

    func retryStart() async {
        working = true
        defer { working = false }
        if case .failure(let error) = await call({ try $0.start() }) {
            startFailure = DevicesCopy.startFailure(error.reason)
        } else {
            startFailure = nil
        }
        await refresh()
    }

    // MARK: Sharing and address

    func setSharing(_ enabled: Bool) async {
        sharingError = nil
        if case .failure(let error) = await call({ try $0.setSharing(enabled) }) {
            sharingError = DevicesCopy.sharingFailure(error.reason)
        }
        await refresh()
    }

    func applyListen(host: String, port: Int, nodeLabel: String) async {
        listenError = nil
        guard Self.portRange.contains(port) else {
            listenError = DevicesCopy.portRangeLine
            return
        }
        guard nodeLabel.utf8.count <= Self.nodeLabelMaxBytes else {
            listenError = DevicesCopy.nameTooLong
            return
        }
        let label = nodeLabel.isEmpty ? nil : nodeLabel
        switch await call({ try $0.listenConfigure(host: host, port: port, nodeLabel: label) }) {
        case .success(let next): status = next
        case .failure(let error):
            listenError = error.reason == "invalid" ? DevicesCopy.addressRefused : DevicesCopy.genericFailure
        }
    }

    // MARK: Scan scope

    func loadDisclosure() async {
        let result = await call({ try $0.scopeDisclosure() })
        guard !Task.isCancelled else { return }
        if case .success(let next) = result { disclosure = next }
    }

    /// Sends `digest`, the one the button rendered with the disclosure on
    /// screen. `invalid` (the inputs changed after it was shown): reload and
    /// ask again; never resent.
    func approveScope(digest: String) async {
        scopeNotice = nil
        switch await call({ try $0.scopeApprove(ownerInputsSha256: digest) }) {
        case .success:
            break
        case .failure(let error) where error.reason == "invalid":
            scopeNotice = DevicesCopy.scopeChanged
            await loadDisclosure()
        case .failure:
            scopeNotice = DevicesCopy.genericFailure
        }
        await refresh()
    }

    // MARK: Invitation (producer)

    /// Only the newest create, and only one the page has not left since,
    /// publishes its offer; an older or orphaned success is cancelled in the
    /// core at once, so no invitation stays claimable out of sight.
    func createOffer() async {
        offerGeneration &+= 1
        let generation = offerGeneration
        offerError = nil
        await clearOffer()?.value
        let ttl = Self.offerTTLSeconds
        let result = await call({ try $0.offerCreate(ttlSeconds: ttl) })
        guard generation == offerGeneration else {
            if case .success(let stale) = result {
                let client = client
                let offerId = stale.offerId
                Task.detached(priority: .utility) { try? client.offerCancel(offerId: offerId) }
            }
            return
        }
        switch result {
        case .success(let next): offer = next
        case .failure(let error): offerError = DevicesCopy.offerFailure(error.reason)
        }
    }

    func copyOffer() {
        guard let offer else { return }
        copiedChangeCount = pasteboard.writeSecret(offer.pairUrl)
        copied = true
    }

    /// Clears the Pair URL now, empties the pasteboard if it still holds
    /// it, and cancels the invitation in the background (the returned task).
    @discardableResult
    func clearOffer() -> Task<Void, Never>? {
        guard let cleared = offer else { return nil }
        offer = nil
        copied = false
        if let copiedChangeCount, pasteboard.changeCount == copiedChangeCount { pasteboard.clear() }
        copiedChangeCount = nil
        let client = client
        let offerId = cleared.offerId
        return Task.detached(priority: .utility) { try? client.offerCancel(offerId: offerId) }
    }

    func expireOfferIfDue(nowMs: Int64) {
        if let offer, nowMs >= offer.expiresAt { clearOffer() }
    }

    // MARK: Requests and grants (producer)

    /// Approve or reject the request on the row the user clicked. The id is
    /// that row's own, the one shown with its SAS: never an index and never
    /// a fresh read of the list (security r1 F1).
    func decide(_ row: RemotePending, approve: Bool) async {
        let pendingId = row.pendingId
        decideError = nil
        deciding.insert(pendingId)
        defer { deciding.remove(pendingId) }
        let result = await call {
            approve ? try $0.approve(pendingId: pendingId) : try $0.reject(pendingId: pendingId)
        }
        if case .failure(let error) = result {
            decideError = error.reason == "expired" ? DevicesCopy.requestExpired : DevicesCopy.genericFailure
        }
        await refresh()
    }

    func revoke(_ grant: RemoteGrant) async {
        manageError = nil
        let grantId = grant.grantId
        if case .failure = await call({ try $0.revoke(grantId: grantId) }) {
            manageError = DevicesCopy.genericFailure
        }
        await refresh()
    }

    // MARK: Linking (receiver)

    func removePeer(_ peer: RemotePeer) async {
        manageError = nil
        let key = peer.producerStatic
        if case .failure = await call({ try $0.peerRemove(producerStatic: key) }) {
            manageError = DevicesCopy.genericFailure
        }
        await refresh()
    }

    /// Claims the pasted invitation, then waits for the producer's approval
    /// on a thread of its own. The text is not kept.
    func link(pairUrl: String) async {
        switch pairing {
        case .claiming, .waiting: return
        default: break
        }
        let text = pairUrl.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text.utf8.count <= Self.pairUrlMaxLength else {
            pairing = .failed(DevicesCopy.invalidInvitation)
            return
        }
        pairing = .claiming
        switch await call({ try $0.pairClaim(pairUrl: text) }) {
        case .failure(let error):
            pairing = .failed(DevicesCopy.linkFailure(error.reason))
        case .success(let claim):
            pairing = .waiting(claim)
            let client = client
            Task { [weak self] in
                let result = await Self.onOwnThread { try client.pairPoll(handle: claim.handle) }
                await self?.pollEnded(claim, result)
            }
        }
    }

    /// Stops showing the wait. The poll runs on to its end; if it then
    /// succeeds, the producer is removed at once (security r1 F2).
    func cancelWait() {
        guard case .waiting(let claim) = pairing else { return }
        cancelledHandles.insert(claim.handle)
        pairing = .idle
    }

    func dismissPairingResult() {
        switch pairing {
        case .linked, .failed: pairing = .idle
        default: break
        }
    }

    private func pollEnded(_ claim: RemoteClaim, _ result: Result<RemotePaired, DevicesFailure>) async {
        let cancelled = cancelledHandles.remove(claim.handle) != nil
        switch result {
        case .success(let paired) where cancelled:
            let key = paired.producerStatic
            if case .failure = await call({ try $0.peerRemove(producerStatic: key) }) {
                manageError = DevicesCopy.cancelledStillLinked
            }
        case .success(let paired):
            pairing = .linked(fingerprint: paired.fingerprint)
        case .failure where cancelled:
            break
        case .failure(let error):
            pairing = .failed(DevicesCopy.linkFailure(error.reason))
        }
        await refresh()
    }

    // MARK: Calls

    struct DevicesFailure: Error {
        /// The core's fixed class string, or `internal`.
        let reason: String

        init(_ error: Error) {
            if case TBCoreError.bridge(let reason) = error { self.reason = reason } else { reason = "internal" }
        }
    }

    /// One blocking client call off the main thread.
    private func call<T: Sendable>(
        _ body: @escaping @Sendable (DevicesRemoteClient) throws -> T
    ) async -> Result<T, DevicesFailure> {
        let client = client
        return await Task.detached(priority: .userInitiated) {
            do { return .success(try body(client)) } catch { return .failure(DevicesFailure(error)) }
        }.value
    }

    /// `pairPoll` blocks up to 960 s: on a thread of its own, not one of the
    /// cooperative pool's.
    nonisolated private static func onOwnThread<T: Sendable>(
        _ body: @escaping @Sendable () throws -> T
    ) async -> Result<T, DevicesFailure> {
        await withCheckedContinuation { continuation in
            Thread {
                do { continuation.resume(returning: .success(try body())) } catch {
                    continuation.resume(returning: .failure(DevicesFailure(error)))
                }
            }.start()
        }
    }
}

/// Devices-page copy, shared by the view and the selftest. English is the
/// catalog key.
enum DevicesCopy {
    static let page = "Devices"

    // (A) Sharing this Mac
    static let shareSection = "Share this Mac"
    static let shareSummary = "Let your other devices read this Mac's usage totals."
    static let service = "Sharing service"
    static let serviceRunning = "Running"
    static let serviceStopped = "Not running"
    static let notRunning = "The sharing service isn't running."
    static let startBusy = "Another copy of Syrtis is using the sharing service."
    static let startUnsafe = "The sharing service's folder can't be used safely."
    static let startFailed = "The sharing service couldn't start."
    static let retry = "Retry"
    static let shareToggle = "Share this Mac's usage"
    static let shareOffNote = "Turning sharing off ends access for every device listed under Devices with access."
    static let needsScope = "Approve the scan scope below before sharing."
    static let sharingBusy = "An earlier pairing invitation hasn't expired yet. Try again in a few minutes."
    static let sharingAlreadyOn = "Sharing is already on."
    static let deviceName = "Device name"
    static let listenOn = "Listen on"
    static let localOnlyLabel = "This Mac only (127.0.0.1)"
    static let chooseAddress = "Choose an address"
    static let port = "Port"
    static let apply = "Apply"
    static let unreachableLine = "Other devices can't reach this Mac."
    static let publicAddress = "This address may be reachable from the internet."
    static let portRangeLine = "Use a port from 1024 to 65535."
    static let nameTooLong = "The name is too long (at most 128 bytes)."
    static let addressRefused = "Syrtis can't listen on this address."
    static let createInvitation = "Create invitation"
    static let invitationReady = "Invitation ready. Copy it and paste it on the other device."
    static let expiresIn = "Expires in %@"
    static let copy = "Copy"
    static let copiedLine = "Copied"
    static let cancelInvitation = "Cancel invitation"
    static let offerNeedsSharing = "Turn on sharing first."
    static let offerNeedsAddress = "Choose a listen address first."
    static let requests = "Waiting for approval"
    static let compareCode = "Check that the other device shows the same code."
    static let approve = "Approve"
    static let reject = "Reject"
    static let approveEffect = "Approving lets that device read this Mac's usage totals in the approved scan scope for 30 days, until you revoke it or it expires."
    static let requestExpired = "This request has expired."
    static let grantsHeader = "Devices with access"
    static let revoke = "Revoke"
    static let revokeTitle = "Revoke access for %@?"
    static let revokeMessage = "That device can no longer read this Mac's usage."
    static let expiresOn = "Expires %@"
    static let cancel = "Cancel"

    // (B) Scan scope
    static let scopeSection = "Scan scope"
    static let scopeSummary = "What sharing reads, and what it sends."
    static let scopeBody = "Sharing sends usage totals: token counts, cost, message and turn counts, and duration, grouped by day and hour (with the time-zone offset) and by client, model, provider and agent name. Those names are sent exactly as recorded on this Mac. Conversation content is not sent, and there are no fields for file paths or accounts. Clients that a Syrtis update adds support for are included automatically."
    static let homeFolder = "Home folder"
    static let configFolder = "Config folder"
    static let dataFolder = "Data folder"
    static let addedFolders = "Added folders"
    static let excludedFolders = "Excluded folders"
    static let openCodeDatabases = "OpenCode databases"
    static let clients = "Clients"
    static let envRoots = "Some clients' folders are set by environment variables (%@). This Mac's pages follow them; sharing doesn't."
    static let scopeApprovedLine = "Approved."
    static let needsApproval = "Needs approval. Nothing is shared until then."
    static let approveScope = "Approve scan scope"
    static let scopeChanged = "The scope changed after it was shown. Review it again, then approve."

    // (C) Linked devices
    static let linkedSection = "Linked devices"
    static let linkedSummary = "Devices whose usage this Mac can read."
    static let noLinked = "No linked devices."
    static let remove = "Remove"
    static let removeTitle = "Remove %@?"
    static let removeMessage = "This Mac stops reading that device's usage. Link it again to read it."
    static let linkAnother = "Link another device"
    static let pastePlaceholder = "Paste the invitation"
    static let link = "Link"
    static let yourCode = "Code: %@"
    static let waitingApproval = "Check that the other device shows the same code and approve it there. Waiting…"
    static let linkedLine = "Linked."
    static let invitationExpired = "The invitation expired. Create a new one on the other device."
    static let declined = "The other device declined."
    static let unreachableDevice = "Syrtis couldn't reach the other device."
    static let identityChanged = "This Mac's sharing identity changed while waiting. Link again."
    static let invalidInvitation = "That isn't a valid invitation."
    static let ownInvitation = "That invitation is from this Mac."
    static let unusableInvitation = "That invitation can't be used anymore."
    static let linkFailed = "Linking didn't finish. Try again."
    static let cancelledStillLinked = "Syrtis couldn't remove a device whose linking you cancelled. Remove it from the list."
    static let stateActive = "Active"
    static let stateRevoked = "Revoked"
    static let stateExpired = "Expired"

    static let genericFailure = "Something went wrong. Try again."

    static func startFailure(_ reason: String) -> String {
        switch reason {
        case "busy": startBusy
        case "unsafeState", "unsafeSocket", "pathTooLong": startUnsafe
        default: startFailed
        }
    }

    static func sharingFailure(_ reason: String) -> String {
        switch reason {
        case "busy": sharingBusy
        case "invalid": sharingAlreadyOn
        default: genericFailure
        }
    }

    static func offerFailure(_ reason: String) -> String {
        switch reason {
        case "unauthorized": offerNeedsSharing
        case "invalid": offerNeedsAddress
        default: genericFailure
        }
    }

    static func linkFailure(_ reason: String) -> String {
        switch reason {
        case "expired": invitationExpired
        case "rejected": declined
        case "transport": unreachableDevice
        case "identityChanged": identityChanged
        case "invalidOffer": invalidInvitation
        case "selfPairing": ownInvitation
        case "offerUnusable", "unauthorized", "revoked": unusableInvitation
        default: linkFailed
        }
    }

    static func state(_ raw: String) -> String {
        switch raw {
        case "active": stateActive
        case "revoked": stateRevoked
        case "expired": stateExpired
        default: raw
        }
    }

    static let all = [
        page, shareSection, shareSummary, service, serviceRunning, serviceStopped, notRunning,
        startBusy, startUnsafe, startFailed, retry, shareToggle, shareOffNote, needsScope,
        sharingBusy, sharingAlreadyOn, deviceName, listenOn, localOnlyLabel, chooseAddress, port,
        apply, unreachableLine, publicAddress, portRangeLine, nameTooLong, addressRefused,
        createInvitation, invitationReady, expiresIn, copy, copiedLine, cancelInvitation,
        offerNeedsSharing, offerNeedsAddress, requests, compareCode, approve, reject, approveEffect,
        requestExpired, grantsHeader, revoke, revokeTitle, revokeMessage, expiresOn, cancel,
        scopeSection, scopeSummary, scopeBody, homeFolder, configFolder, dataFolder, addedFolders,
        excludedFolders, openCodeDatabases, clients, envRoots, scopeApprovedLine, needsApproval,
        approveScope, scopeChanged, linkedSection, linkedSummary, noLinked, remove, removeTitle,
        removeMessage, linkAnother, pastePlaceholder, link, yourCode, waitingApproval, linkedLine,
        invitationExpired, declined, unreachableDevice, identityChanged, invalidInvitation,
        ownInvitation, unusableInvitation, linkFailed, cancelledStillLinked, stateActive,
        stateRevoked, stateExpired, genericFailure,
    ]
}
