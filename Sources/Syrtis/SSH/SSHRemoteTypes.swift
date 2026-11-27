import CryptoKit
import Darwin
import Foundation
import os
import TokenBarCore

// SSH remote machines (docs/knowledge/plans/ssh-remote-machines.md): value
// types, input validation, the strict output frame and the fixed remote command
// strings. Nothing here opens a connection.
//
// Every value that reaches an `ssh` argv or the remote shell goes through the
// checks below first, and the remote command is one of a small set of
// program-owned strings: a caller cannot hand `SSHRunner` free text.

let sshLog = Logger(subsystem: "com.nyanako.tokenbar", category: "ssh-remote")

// MARK: Errors

/// Failures of the SSH remote machine flows. The payloads are class strings or
/// the first stderr line with `syrtis://` masked: UI text only, never logged
/// publicly.
enum SSHRemoteError: Error, Equatable, Sendable {
    /// A field failed its format check (the field name, never the value).
    case invalidInput(String)
    case launchFailed
    /// The host key is unknown or changed; the owner confirms it in a terminal.
    case hostKeyRejected
    case unreachable(String)
    case timedOut
    case outputLimit
    case cancelled
    /// stdout was not exactly one `SYRTIS-CTL-V1 {json}` line.
    case malformedOutput
    /// `uname -sm` is not in the fixed table.
    case unsupportedPlatform
    /// The pinned agent sha256 is still the placeholder.
    case agentPinMissing
    /// The binary's sha256 differs from the pin (local or remote check).
    case integrity
    /// The remote has no agent binary where `install` puts it.
    case agentMissing
    /// The uploaded agent cannot execute on the remote (wrong libc or kernel).
    case agentCannotRun(String)
    /// `ctl` or the deploy script exited with `code` (spec/cli-v1.md).
    case ctl(code: Int32, detail: String)
    /// Existing directories the agent refuses (group or other writable), as `~`-relative paths.
    case unsafeRemoteDirs([String])
    case busy
    case portInUse(Int)
    case noFreePort
    case declined
    /// The remote agent listens on a non-loopback address another consumer may
    /// have pinned; moving it is not done here.
    case listenerNotLoopback
    case tunnelNotUp(String)
    /// A local `tb_remote_*` call failed with its fixed class string.
    case pairing(String)
    case identityChanged
    /// The scan scope was cut for display (`SSHDisclosure.truncated`); the
    /// approval covers all of it, so it is never approved from a partial view.
    case disclosureTruncated

    /// The failure says nothing about the remote's state: no connection was
    /// made, or it is unknown whether the command ran.
    var isConnectionFailure: Bool {
        switch self {
        case .unreachable, .hostKeyRejected, .timedOut: true
        default: false
        }
    }
}

// MARK: Validation

enum SSHInput {
    static let portRange = 47100...47999

    private static func allAllowed(_ text: String, _ allowed: (UInt8) -> Bool) -> Bool {
        !text.isEmpty && text.utf8.allSatisfy(allowed)
    }

    private static func isNameByte(_ b: UInt8) -> Bool {
        (b >= 0x30 && b <= 0x39) || (b >= 0x41 && b <= 0x5A) || (b >= 0x61 && b <= 0x7A)
            || b == 0x2E || b == 0x5F || b == 0x2D
    }

    /// `^[A-Za-z0-9._-]+$`, not starting with `-` (so never an ssh option).
    static func isName(_ text: String, maxLength: Int) -> Bool {
        text.utf8.count <= maxLength && allAllowed(text, isNameByte) && !text.hasPrefix("-")
    }

    static func isUser(_ text: String) -> Bool { isName(text, maxLength: 64) }
    static func isHost(_ text: String) -> Bool { isName(text, maxLength: 253) }

    static func isHex(_ text: String, count: Int) -> Bool {
        text.utf8.count == count
            && text.utf8.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x61 && $0 <= 0x66) }
    }

    /// `^[0-9a-f]{64}$`.
    static func isDigest(_ text: String) -> Bool { isHex(text, count: 64) }
    /// 32 lowercase hex: offerId, pendingId, grantId.
    static func isId32(_ text: String) -> Bool { isHex(text, count: 32) }
    static func isFingerprint(_ text: String) -> Bool { isHex(text, count: 16) }

    /// `^[0-9A-F]{4}-[0-9A-F]{4}$`.
    static func isSAS(_ text: String) -> Bool {
        let bytes = Array(text.utf8)
        guard bytes.count == 9, bytes[4] == 0x2D else { return false }
        return bytes.enumerated().allSatisfy { index, b in
            index == 4 || (b >= 0x30 && b <= 0x39) || (b >= 0x41 && b <= 0x46)
        }
    }

    static func isPort(_ port: Int, in range: ClosedRange<Int> = 1024...65535) -> Bool {
        range.contains(port)
    }

    /// Canonical-looking IP text only (what `ctl listen --host` takes).
    static func isIPText(_ text: String) -> Bool {
        guard text.utf8.count <= 45, !text.isEmpty, text.utf8.allSatisfy({
            ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x41 && $0 <= 0x46) || ($0 >= 0x61 && $0 <= 0x66)
                || $0 == 0x2E || $0 == 0x3A
        }) else { return false }
        var v4 = in_addr()
        var v6 = in6_addr()
        return inet_pton(AF_INET, text, &v4) == 1 || inet_pton(AF_INET6, text, &v6) == 1
    }

    /// Display name: control and bidi-override characters out, at most 64
    /// characters, blank is nil.
    static func cleanedLabel(_ typed: String) -> String? {
        let kept = typed.unicodeScalars.filter { scalar in
            !CharacterSet.controlCharacters.contains(scalar)
                && !(0x202A...0x202E).contains(scalar.value)
                && !(0x2066...0x2069).contains(scalar.value)
                && !(0x200B...0x200F).contains(scalar.value)
        }
        let text = String(String.UnicodeScalarView(kept)).trimmingCharacters(in: .whitespaces)
        return text.isEmpty ? nil : String(text.prefix(64))
    }

    /// Replaces every `syrtis://...` token and drops control characters; at
    /// most 200 characters. For text shown in the UI.
    static func redacted(_ line: String) -> String {
        var out = ""
        var rest = Substring(line)
        while let range = rest.range(of: "syrtis:", options: .caseInsensitive) {
            out += rest[rest.startIndex..<range.lowerBound]
            out += "syrtis://[redacted]"
            rest = rest[range.upperBound...].drop { !$0.isWhitespace }
        }
        out += rest
        let clean = out.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }
        return String(String(String.UnicodeScalarView(clean)).prefix(200))
    }

    /// First non-empty line of stderr, masked.
    static func firstLine(_ data: Data) -> String {
        let text = String(decoding: data, as: UTF8.self)
        let line = text.split(whereSeparator: \.isNewline).first.map(String.init) ?? ""
        return redacted(line)
    }

    /// True when ssh's stderr, any line of it, says the host key was rejected
    /// (OpenSSH prints why on the line before "verification failed").
    static func saysHostKeyRejected(_ text: String) -> Bool {
        let lowered = text.lowercased()
        return lowered.contains("host key verification failed")
            || lowered.contains("remote host identification has changed")
    }

    static func saysHostKeyRejected(_ data: Data) -> Bool {
        saysHostKeyRejected(String(decoding: data, as: UTF8.self))
    }
}

// MARK: Machine and target

struct SSHTarget: Equatable, Hashable, Sendable {
    /// nil user and port are left to `~/.ssh/config`: no `user@` and no `-p`.
    let user: String?
    let host: String
    let port: Int?

    init(user: String? = nil, host: String, port: Int? = nil) throws {
        if let user { guard SSHInput.isUser(user) else { throw SSHRemoteError.invalidInput("user") } }
        guard SSHInput.isHost(host) else { throw SSHRemoteError.invalidInput("host") }
        if let port { guard (1...65535).contains(port) else { throw SSHRemoteError.invalidInput("port") } }
        self.user = user
        self.host = host
        self.port = port
    }

    var destination: String { user.map { "\($0)@\(host)" } ?? host }
    var portArguments: [String] { port.map { ["-p", String($0)] } ?? [] }
}

/// One saved machine. No secret: the Pair URL, the control token and the
/// producer's keys' private halves never reach this record.
struct SSHMachine: Codable, Equatable, Identifiable, Sendable {
    var id: UUID
    var label: String
    var user: String?
    var host: String
    var sshPort: Int?
    var remotePort: Int
    var localPort: Int
    /// `responderStatic` of the paired producer (64 hex), nil before pairing.
    var producerStatic: String?
    /// The same key's 16-hex fingerprint (`RemoteClaim.producerFingerprint`).
    var fingerprint: String?
    /// The remote grant of this receiver (32 hex), derived from the
    /// `pendingId` `ctl pending approve` returned. nil before pairing.
    var grantId: String?
    var enabled: Bool

    func target() throws -> SSHTarget { try SSHTarget(user: user, host: host, port: sshPort) }

    /// `user@host`, or the bare ssh alias or host when there is no user. The port is not shown.
    var addressText: String { user.map { "\($0)@\(host)" } ?? host }

    /// Re-checks every field of a loaded record; nil when any is invalid.
    func validated() -> SSHMachine? {
        guard (try? target()) != nil,
              SSHInput.isPort(remotePort), SSHInput.portRange.contains(localPort),
              producerStatic.map(SSHInput.isDigest) ?? true,
              fingerprint.map(SSHInput.isFingerprint) ?? true,
              grantId.map(SSHInput.isId32) ?? true,
              let clean = SSHInput.cleanedLabel(label)
        else { return nil }
        var copy = self
        copy.label = clean
        return copy
    }
}

/// A flow failed and some of its undo steps could not run. `cause` is why the
/// flow failed; `commands` finish the undo by hand.
struct SSHRollbackIncomplete: Error {
    let cause: Error
    let commands: [String]
}

/// Terminal lines for undo steps that could not run: each is the exact argv the
/// app passes ssh, quoted for a shell.
enum SSHManualCommands {
    static func lines(_ commands: [SSHCtlCommand], on target: SSHTarget) -> [String] {
        let prefix = (["ssh"] + target.portArguments + ["--", target.destination]).joined(separator: " ") + " "
        return commands.compactMap { try? $0.remoteCommand() }.map { prefix + SSHRemoteCommand.terminalQuoted($0.shell) }
    }
}

// MARK: Remote commands

/// The program-owned remote command line: `sh -c '<fixed script>'`. The only
/// ways to make one are the builders below, which insert nothing but values
/// that passed their format check.
struct SSHRemoteCommand: Equatable, Sendable {
    let shell: String

    fileprivate init(script: String) throws {
        guard !script.contains("'"), !script.contains("\n") else {
            throw SSHRemoteError.invalidInput("command")
        }
        shell = "sh -c '\(script)'"
    }

    static let binary = "\"$HOME/.local/bin/syrtis-agent\""

    /// `text` as one word for a POSIX shell: single quotes, with each `'`
    /// closed, escaped and reopened.
    static func terminalQuoted(_ text: String) -> String {
        "'" + text.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    static let uname = try! SSHRemoteCommand(script: "uname -sm")

    /// The directories `syrtis-agent ctl install` requires to be owner-only
    /// writable when they exist (`$HOME` is the empty suffix).
    static let agentDirSuffixes = [
        "", "/.local", "/.local/state", "/.local/bin", "/.config", "/.config/systemd", "/.config/systemd/user",
    ]

    /// Read-only: prints `~<suffix>` for each existing directory whose group or
    /// other write bit is set (`ls -ld` mode string, symlinks followed).
    static let permissionCheck = try! SSHRemoteCommand(script: [
        "for p in \(agentDirSuffixes.map { "\"\($0)\"" }.joined(separator: " "))",
        "do d=\"$HOME$p\"; [ -d \"$d\" ] || continue; m=$(ls -ld \"$d/.\" 2>/dev/null)",
        "case \"$m\" in ?????w*|????????w*) printf \"%s\\n\" \"~$p\";; esac; done",
    ].joined(separator: "; "))

    /// systemd user service when it works, otherwise a detached `run` with all
    /// stdio redirected (nothing may hang ssh or leak into the frame).
    static let startFragment =
        "if command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload >/dev/null 2>&1 "
        + "&& systemctl --user enable --now syrtis-agent.service >/dev/null 2>&1; then :; "
        + "else setsid nohup \(binary) run </dev/null >/dev/null 2>&1 & fi"

    /// Starts the installed agent; prints nothing.
    static let start = try! SSHRemoteCommand(script: startFragment)

    /// Upload from stdin, hash, install, start, clean up. `pin` is 64 lowercase
    /// hex (checked here). The temp directory is 0700 under `$HOME` and never
    /// reported back. Exit 70 mktemp, 71 upload, 72 hash mismatch, 73 the binary
    /// cannot run here (`ctl install` exited 1, 126 or 127, none of them a `ctl`
    /// status, e.g. a loader error; the
    /// first line of its stderr is passed on), else `ctl install`'s status; stdout is `ctl install`'s frame.
    static func deploy(pin: String) throws -> SSHRemoteCommand {
        guard SSHInput.isDigest(pin) else { throw SSHRemoteError.invalidInput("pin") }
        let script = [
            "umask 077",
            "T=$(mktemp -d \"$HOME/.syrtis-up.XXXXXX\") || exit 70",
            "trap \"rm -rf \\\"$T\\\"\" EXIT",
            "cat > \"$T/a\" || exit 71",
            "H=$( (sha256sum \"$T/a\" || shasum -a 256 \"$T/a\") 2>/dev/null | cut -d\" \" -f1)",
            "[ \"$H\" = \"\(pin)\" ] || exit 72",
            "chmod 700 \"$T/a\"",
            "\"$T/a\" ctl shutdown >/dev/null 2>&1",
            "i=0",
            "while [ $i -lt 20 ]; do OUT=$(\"$T/a\" ctl install 2>\"$T/e\"); rc=$?; "
                + "[ $rc -eq 9 ] || break; i=$((i+1)); sleep 1; done",
            "case $rc in 1|126|127) head -n 1 \"$T/e\" >&2; exit 73;; esac",
            "[ $rc -eq 0 ] || { head -n 1 \"$T/e\" >&2; exit $rc; }",
            startFragment,
            "printf \"%s\\n\" \"$OUT\"",
        ].joined(separator: "; ")
        return try SSHRemoteCommand(script: script)
    }

    // MARK: Price table

    /// The remote agent's pricing directory exists, is a real directory (not a link) owned by the
    /// remote user; otherwise exit 74. Nothing creates it: an agent that never ran has none.
    private static let pricingDirCheck =
        "D=\"$HOME/.local/state/syrtis-agent/pricing\"; [ -d \"$D\" ] && [ ! -L \"$D\" ] && [ -O \"$D\" ] || exit \(SSHPricingExit.noDirectory)"

    /// Read-only: `<sha256>  <name>` for each of the two files that exists, after the
    /// directory checks; exit 75 when a name is a directory or a link to one.
    static let pricingHashes = try! SSHRemoteCommand(script: [
        pricingDirCheck,
        "for n in \(SSHPricingFile.allCases.map(\.rawValue).joined(separator: " ")); do "
            + "[ -d \"$D/$n\" ] && exit \(SSHPricingExit.nameIsDirectory); [ -f \"$D/$n\" ] || continue; "
            + "H=$( (sha256sum \"$D/$n\" || shasum -a 256 \"$D/$n\") 2>/dev/null | cut -d\" \" -f1); "
            + "printf \"%s  %s\\n\" \"$H\" \"$n\"; done",
    ].joined(separator: "; "))

    /// Upload one price file from stdin into a temp file in the pricing directory, check its
    /// sha256 against `digest`, and rename it over `<name>` (same directory). Exit 74 or 75 as
    /// `pricingHashes`, 70 mktemp, 71 upload, 72 hash mismatch; the temp file is removed on exit.
    static func pricingUpload(_ file: SSHPricingFile, digest: String) throws -> SSHRemoteCommand {
        guard SSHInput.isDigest(digest) else { throw SSHRemoteError.invalidInput("digest") }
        let name = file.rawValue
        let script = [
            "umask 077",
            pricingDirCheck,
            "[ -d \"$D/\(name)\" ] && exit \(SSHPricingExit.nameIsDirectory)",
            "T=$(mktemp \"$D/.up.XXXXXX\") || exit \(SSHPricingExit.tempFile)",
            "trap \"rm -f \\\"\\$T\\\"\" EXIT",
            "cat > \"$T\" || exit \(SSHPricingExit.upload)",
            "H=$( (sha256sum \"$T\" || shasum -a 256 \"$T\") 2>/dev/null | cut -d\" \" -f1)",
            "[ \"$H\" = \"\(digest)\" ] || exit \(SSHPricingExit.mismatch)",
            "mv -f \"$T\" \"$D/\(name)\"",
        ].joined(separator: "; ")
        return try SSHRemoteCommand(script: script)
    }
}

/// The two price files the engine reads (`RemotePricingSnapshot::from_cache_root`, the same names as
/// `PRICING_FILES` in `remote_agent.rs`). The only names a price command may carry.
enum SSHPricingFile: String, CaseIterable, Sendable {
    case litellm = "pricing-litellm.json"
    case openrouter = "pricing-openrouter.json"
}

/// Exit codes of the price commands. 70-72 mean what they mean for `deploy`.
enum SSHPricingExit {
    static let tempFile: Int32 = 70
    static let upload: Int32 = 71
    static let mismatch: Int32 = 72
    /// No usable `pricing/` (missing, a link, or not the remote user's): skip, create nothing.
    static let noDirectory: Int32 = 74
    /// `pricing/<name>` is a directory or a link to one: `mv` would write into it, so skip.
    static let nameIsDirectory: Int32 = 75
}

/// One `syrtis-agent ctl` invocation (the agent's `ctl` contract).
enum SSHCtlCommand: Equatable, Sendable {
    case status
    case install
    case uninstall(purge: Bool)
    case scopeDisclosure
    case scopeApprove(digest: String)
    case sharing(Bool)
    case listen(host: String, port: Int)
    case offerCreate(ttl: Int)
    case offerCancel(offerId: String)
    case pendingApprove(sas: String)
    case grantRevoke(grantId: String)
    case shutdown

    func remoteCommand() throws -> SSHRemoteCommand {
        func need(_ ok: Bool, _ field: String) throws {
            if !ok { throw SSHRemoteError.invalidInput(field) }
        }
        let args: String
        switch self {
        case .status: args = "status"
        case .install: args = "install"
        case .uninstall(let purge):
            // The start enables the user unit, which leaves a `default.target.wants` link; `ctl uninstall`
            // removes only the unit file, so once it succeeded the link is dropped here. `disable` works by
            // name with the unit file gone (measured on Ubuntu 22.04). The status and the frame are `ctl`'s.
            let uninstall = "\(SSHRemoteCommand.binary) ctl " + (purge ? "uninstall --purge" : "uninstall")
            return try SSHRemoteCommand(script: uninstall + "; rc=$?; [ $rc -ne 0 ] || { command -v systemctl >/dev/null 2>&1 "
                + "&& systemctl --user disable syrtis-agent.service >/dev/null 2>&1; }; exit $rc")
        case .scopeDisclosure: args = "scope disclosure"
        case .scopeApprove(let digest):
            try need(SSHInput.isDigest(digest), "digest")
            args = "scope approve --digest \(digest)"
        case .sharing(let on): args = on ? "sharing on" : "sharing off"
        case .listen(let host, let port):
            try need(SSHInput.isIPText(host), "listen host")
            try need(SSHInput.isPort(port), "listen port")
            args = "listen --host \(host) --port \(port)"
        case .offerCreate(let ttl):
            try need((1...120).contains(ttl), "ttl")
            args = "offer create --ttl \(ttl)"
        case .offerCancel(let offerId):
            try need(SSHInput.isId32(offerId), "offer id")
            args = "offer cancel --offer-id \(offerId)"
        case .pendingApprove(let sas):
            try need(SSHInput.isSAS(sas), "SAS")
            args = "pending approve --expect-sas \(sas)"
        case .grantRevoke(let grantId):
            try need(SSHInput.isId32(grantId), "grant id")
            args = "grant revoke --grant-id \(grantId)"
        case .shutdown: args = "shutdown"
        }
        return try SSHRemoteCommand(script: "\(SSHRemoteCommand.binary) ctl \(args)")
    }
}

// MARK: Frame

/// The one stdout line of a successful `ctl`: `SYRTIS-CTL-V1 {json}\n`.
/// Anything else on stdout (a shell startup file's text, a second line, a
/// carriage return, a missing newline) is a parse failure.
enum SSHFrame {
    static let prefix = "SYRTIS-CTL-V1 "

    static func decode<T: Decodable>(_ type: T.Type, from stdout: Data) throws -> T {
        guard let text = String(data: stdout, encoding: .utf8),
              text.hasSuffix("\n"), text.utf8.filter({ $0 == 0x0A }).count == 1,
              !text.utf8.contains(0x0D), text.hasPrefix(prefix)
        else { throw SSHRemoteError.malformedOutput }
        let json = text.dropFirst(prefix.count).dropLast()
        guard json.hasPrefix("{"), let value = try? JSONDecoder().decode(T.self, from: Data(json.utf8))
        else { throw SSHRemoteError.malformedOutput }
        return value
    }
}

/// `uname -sm` to a target triple, by fixed table only; the remote's text is
/// never used to build a path.
enum SSHPlatform {
    static let table: [String: String] = ["Linux x86_64\n": "x86_64-unknown-linux-gnu"]

    static func triple(fromUname stdout: Data) throws -> String {
        guard let text = String(data: stdout, encoding: .utf8), let triple = table[text] else {
            throw SSHRemoteError.unsupportedPlatform
        }
        return triple
    }
}

// MARK: Agent pin

enum SSHAgentPin {
    static let triple = "x86_64-unknown-linux-gnu"

    /// A valid-looking value that matches no real file: with no pin in the
    /// bundle the deploy check fails closed instead of being skipped.
    static let placeholder = String(repeating: "0", count: 64)

    /// The sha256 of the bundled agent, read from the `syrtis-agent.sha256`
    /// that `make remote-agent` writes beside the binary in the same step that
    /// verified or hashed it, so the two cannot drift. No file (a build made
    /// without `make remote-agent`) or a malformed one is the placeholder.
    static var sha256: String { pin() }

    static func isPlaceholder(_ pin: String) -> Bool { pin == placeholder }

    static func directory(triple: String, resources: URL?) -> URL? {
        guard SSHPlatform.table.values.contains(triple) else { return nil }
        return resources?.appendingPathComponent("remote-agent/\(triple)")
    }

    /// `Contents/Resources/remote-agent/<triple>/syrtis-agent`; nil unless the
    /// file is there.
    static func bundledBinary(triple: String, resources: URL? = Bundle.main.resourceURL) -> URL? {
        guard let url = directory(triple: triple, resources: resources)?.appendingPathComponent("syrtis-agent"),
              FileManager.default.isReadableFile(atPath: url.path)
        else { return nil }
        return url
    }

    static func pin(resources: URL? = Bundle.main.resourceURL) -> String {
        guard let url = directory(triple: triple, resources: resources)?.appendingPathComponent("syrtis-agent.sha256"),
              let data = try? Data(contentsOf: url), data.count <= 256,
              let word = String(decoding: data, as: UTF8.self).split(whereSeparator: \.isWhitespace).first,
              SSHInput.isDigest(String(word))
        else { return placeholder }
        return String(word)
    }

    /// True when the app carries an agent binary and its pin.
    static func isBundled(resources: URL? = Bundle.main.resourceURL) -> Bool {
        bundledBinary(triple: triple, resources: resources) != nil
            && !isPlaceholder(pin(resources: resources))
    }

    /// sha256 of a file, streamed.
    static func sha256(of url: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }
}

/// `grantId = SHA-256("syrtis-pair-grant-v1\0" || pendingId)[..16]`, from the
/// `pendingId` that `ctl pending approve` answers (spec/pairing-v1.md,
/// "Derivations"). The hash covers the 16 raw bytes, not the hex text.
enum SSHGrantId {
    static func derive(pendingId: String) -> String? {
        guard SSHInput.isId32(pendingId) else { return nil }
        var bytes = Data("syrtis-pair-grant-v1".utf8)
        bytes.append(0)
        var index = pendingId.startIndex
        while index < pendingId.endIndex {
            let next = pendingId.index(index, offsetBy: 2)
            guard let byte = UInt8(pendingId[index..<next], radix: 16) else { return nil }
            bytes.append(byte)
            index = next
        }
        return SHA256.hash(data: bytes).prefix(16).map { String(format: "%02x", $0) }.joined()
    }
}

// MARK: Ctl replies

/// The agent on the machine before a flow touched it.
struct SSHAgentBaseline: Equatable, Sendable {
    let installed: Bool
    let running: Bool

    static let missing = SSHAgentBaseline(installed: false, running: false)
}

/// `ctl status`: the baseline a flow compares against.
struct SSHRemoteStatus: Decodable, Equatable, Sendable {
    let sharingEnabled: Bool
    let bindHost: String?
    let bindPort: Int?
    let nodeLabel: String?
    let activeGrantCount: Int
    let activeOfferCount: Int
    let pendingCount: Int
    let scopeApproved: Bool
}

/// `ctl scope disclosure`, bounded for display (`Text(verbatim:)`).
struct SSHDisclosure: Equatable, Sendable {
    struct Line: Equatable, Sendable {
        let name: String
        let value: String
    }

    static let maxLines = 64
    static let maxName = 64
    static let maxValue = 512

    let lines: [Line]
    /// Something was cut for display (more than `maxLines` lines, a name over
    /// `maxName` or a value over `maxValue` characters). The digest covers the
    /// whole scope, so a cut list is never approved.
    let truncated: Bool
    /// Approved verbatim: the digest the owner saw, never a re-fetched one.
    let digest: String

    fileprivate struct Wire: Decodable {
        struct WireLine: Decodable {
            let name: String
            let value: String
        }

        let lines: [WireLine]
        let ownerInputsSha256: String
    }

    init(lines: [Line], digest: String, truncated: Bool = false) {
        self.lines = lines
        self.digest = digest
        self.truncated = truncated
    }

    init(decoding stdout: Data) throws {
        let wire = try SSHFrame.decode(Wire.self, from: stdout)
        guard SSHInput.isDigest(wire.ownerInputsSha256) else { throw SSHRemoteError.malformedOutput }
        var cut = wire.lines.count > Self.maxLines
        func bound(_ text: String, _ limit: Int) -> String {
            let clean = String(String.UnicodeScalarView(
                text.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }))
            if clean.count > limit || clean != text { cut = true }
            return String(clean.prefix(limit))
        }
        let lines = wire.lines.prefix(Self.maxLines).map {
            Line(name: bound($0.name, Self.maxName), value: bound($0.value, Self.maxValue))
        }
        self.init(lines: lines, digest: wire.ownerInputsSha256, truncated: cut)
    }
}
