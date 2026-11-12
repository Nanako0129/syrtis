import Foundation

// Payloads of the `tb_remote_*` entry points (ctb.h). Field names are the
// Rust JSON keys; times are Unix milliseconds unless named otherwise.

/// `tb_remote_status`, `tb_remote_start`, `tb_remote_listen_configure`.
/// Only `running` is present while the agent is stopped.
public struct RemoteAgentStatus: Decodable, Equatable, Sendable {
    public let running: Bool
    public let sharingEnabled: Bool?
    public let scopeApproved: Bool?
    public let listening: Bool?
    public let identityFingerprint: String?
    public let bindHost: String?
    public let bindPort: Int?
    public let nodeLabel: String?
    public let activeGrantCount: Int?
    public let activeOfferCount: Int?
    public let pendingCount: Int?
    /// Unix seconds of the last successful scan for a peer.
    public let lastScanAt: Int64?
    /// `never`, `ok` or `error`.
    public let scanStatus: String?
    public let sourceFamilies: [String]?
}

/// `tb_remote_scope_disclosure`: what sharing would scan, and the digest the
/// owner approves.
public struct RemoteScopeDisclosure: Decodable, Equatable, Sendable {
    public struct EnvRoot: Decodable, Equatable, Sendable {
        public let client: String
        public let variable: String
    }

    public let home: String
    public let configDir: String
    public let dataDir: String
    public let extraScanPaths: [String: [String]]
    public let excludedScanPaths: [String: [String]]
    public let opencodeDbPaths: [String]
    public let clients: [String]
    public let ownerInputsSha256: String
    /// Per-client root variables set in this process: the local pages follow
    /// them, the shared source does not.
    public let envRootsSet: Bool
    public let envRoots: [EnvRoot]
}

/// `tb_remote_offer_create`. `pairUrl` is a secret: the descriptions below
/// leave it out, so string interpolation and logging cannot print it.
public struct RemoteOffer: Decodable, Equatable, Sendable, CustomStringConvertible,
    CustomDebugStringConvertible
{
    public let expiresAt: Int64
    public let offerId: String
    public let pairUrl: String

    public var description: String { "RemoteOffer(offerId: \(offerId), expiresAt: \(expiresAt))" }
    public var debugDescription: String { description }
}

/// One entry of `tb_remote_pending_list`.
public struct RemotePending: Decodable, Equatable, Sendable {
    public let expiresAt: Int64
    public let pendingId: String
    public let sasCode: String
}

/// One entry of `tb_remote_grants`. `state`: `active`, `revoked`, `expired`.
public struct RemoteGrant: Decodable, Equatable, Sendable {
    public let grantId: String
    public let peerFingerprint: String
    public let createdAt: Int64
    public let expiresAt: Int64
    public let state: String
}

/// `tb_remote_pair_claim`: compare `sas` with the producer's screen.
public struct RemoteClaim: Decodable, Equatable, Sendable {
    public let handle: Int64
    public let sas: String
    public let expiresAt: Int64
    public let producerFingerprint: String
}

/// `tb_remote_pair_poll` on approval.
public struct RemotePaired: Decodable, Equatable, Sendable {
    public let producerStatic: String
    public let fingerprint: String
    public let expiresAt: Int64
}

/// One entry of `tb_remote_peers`. `state`: `active`, `revoked`, `expired`.
public struct RemotePeer: Decodable, Equatable, Sendable {
    public let producerStatic: String
    public let fingerprint: String
    public let nodeLabel: String?
    /// The producer's address, from the offer this receiver claimed.
    public let host: String
    public let port: Int
    public let createdAt: Int64
    public let expiresAt: Int64
    public let state: String
}

/// The `null` data of the commands that answer nothing.
public struct RemoteNothing: Decodable, Sendable {}

// MARK: Device-aware pages (`tb_*_devices`)

/// Which sources a device-aware page folds: this Mac and paired producers
/// (`producerStatic` of `tb_remote_peers`, 64 lowercase hex).
public struct DeviceSelection: Encodable, Equatable, Sendable {
    public let local: Bool
    public let peers: [String]

    public init(local: Bool, peers: [String]) {
        self.local = local
        self.peers = peers
    }
}

/// `cached`: the local part and each producer's last good body, no
/// connection. `refresh`: each selected producer is pulled.
public enum DeviceFetchMode: String, Sendable {
    case cached
    case refresh
}

/// One `devices` row. `id` is `local`, `peers` (the peer store could not be
/// read) or a producer's 16-hex fingerprint; `state` is one of `local`,
/// `fresh`, `lastGood`, `stale`, `unavailable`, `refused`, `invalidReply`,
/// `incompatible`; `lastSuccessAt` is Unix seconds of the last fresh pull in
/// this app run.
public struct RemoteDevice: Decodable, Equatable, Sendable {
    public let id: String
    public let label: String?
    public let state: String
    public let lastSuccessAt: Int64?
}

/// A page payload with paired devices folded in by the core, and the device
/// rows. Deliberately not the page's own type (`UsagePayload`,
/// `ModelReport`, …): data that may hold another machine's usage must never
/// reach `SnapshotStore` or replace the local payload the tray and the
/// snapshot use.
public struct DeviceAwarePayload<Payload: Decodable & Sendable>: Decodable, Sendable {
    public let payload: Payload
    public let devices: [RemoteDevice]
    /// Two or more sources: synced history may be counted more than once.
    public let duplicateWarning: Bool
    /// Some sum reached its integer limit and was capped.
    public let saturated: Bool
    /// All time: producers answered only the last 370 days.
    public let remoteRangeLimited: Bool

    private enum CodingKeys: String, CodingKey {
        case devices, duplicateWarning, saturated, remoteRangeLimited
    }

    public init(from decoder: Decoder) throws {
        payload = try Payload(from: decoder)
        let container = try decoder.container(keyedBy: CodingKeys.self)
        devices = try container.decode([RemoteDevice].self, forKey: .devices)
        duplicateWarning = try container.decode(Bool.self, forKey: .duplicateWarning)
        saturated = try container.decode(Bool.self, forKey: .saturated)
        remoteRangeLimited = try container.decode(Bool.self, forKey: .remoteRangeLimited)
    }
}
