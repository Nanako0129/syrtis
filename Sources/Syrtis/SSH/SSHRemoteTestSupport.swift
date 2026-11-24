import Foundation
import TokenBarCore

// Test doubles for the SSH remote machine selftests (`SSHRemoteSelfTest`,
// `SSHRemoteIntegrationTest`): a fake `ssh` script that logs its argv and runs
// the remote command through /bin/sh against a fake `syrtis-agent` script with
// a state directory, so the real fixed command strings, frames and exit codes
// are exercised. Not used by the app.

final class SSHFake: @unchecked Sendable {
    let dir: URL
    let home: URL
    let state: URL
    let bin: URL
    let sshScript: URL
    let agentScript: URL
    let agentSHA256: String

    /// `agentBinary`: a real binary to upload instead of the fake agent script
    /// (integration: a debug build of the agent with `ctl`). `short`: a short
    /// directory (the real agent's control socket path limit).
    init(agentBinary: URL? = nil, short: Bool = false, realAgentHome: Bool = false) throws {
        let base = short ? "/tmp/sit\(getpid())"
            : FileManager.default.temporaryDirectory.path + "/syrtis-sshfake-\(UUID().uuidString)"
        dir = URL(fileURLWithPath: base)
        home = dir.appendingPathComponent("h")
        state = dir.appendingPathComponent("s")
        bin = dir.appendingPathComponent("bin")
        sshScript = dir.appendingPathComponent("ssh")
        agentScript = dir.appendingPathComponent("agent")
        let fm = FileManager.default
        for url in [dir, home, state, bin] {
            try fm.createDirectory(at: url, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        }
        try Self.fakeAgent(state: state.path).write(to: agentScript, atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: agentScript.path)
        try "#!/bin/sh\nexec \"$@\"\n".write(
            to: bin.appendingPathComponent("setsid"), atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bin.appendingPathComponent("setsid").path)
        // A Linux x86_64 answer for `uname -sm`, and the agent where `install` would put it.
        let uname = bin.appendingPathComponent("uname")
        try "#!/bin/sh\necho 'Linux x86_64'\n".write(to: uname, atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: uname.path)
        if agentBinary == nil {
            let installed = home.appendingPathComponent(".local/bin")
            try fm.createDirectory(at: installed, withIntermediateDirectories: true)
            // Whatever the umask: the agent refuses group/other-writable directories.
            for url in [installed.deletingLastPathComponent(), installed] {
                try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: url.path)
            }
            try fm.copyItem(at: agentScript, to: installed.appendingPathComponent("syrtis-agent"))
        }
        let env = realAgentHome ? " SYRTIS_AGENT_DEBUG_HOME=\"\(home.path)\"" : ""
        try Self.fakeSSH(dir: dir.path, home: home.path, bin: bin.path, extraEnv: env)
            .write(to: sshScript, atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: sshScript.path)
        agentSHA256 = try SSHAgentPin.sha256(of: agentBinary ?? agentScript)
        uploaded = agentBinary ?? agentScript
    }

    let uploaded: URL

    var runner: SSHRunner {
        var runner = SSHRunner()
        runner.sshPath = sshScript.path
        runner.extraEnvironment = ["PATH": "\(bin.path):/usr/bin:/bin:/usr/sbin:/sbin", "HOME": home.path]
        return runner
    }

    func cleanup() { try? FileManager.default.removeItem(at: dir) }

    // MARK: Control

    func setMode(_ mode: String?) {
        let url = dir.appendingPathComponent("mode")
        if let mode { try? mode.write(to: url, atomically: true, encoding: .utf8) }
        else { try? FileManager.default.removeItem(at: url) }
    }

    func setDown(_ down: Bool) {
        let url = dir.appendingPathComponent("down")
        if down { FileManager.default.createFile(atPath: url.path, contents: Data()) }
        else { try? FileManager.default.removeItem(at: url) }
    }

    var installedAgent: URL { home.appendingPathComponent(".local/bin/syrtis-agent") }

    /// The agent binary on the machine: a different build, or none (nil).
    func setInstalled(otherBuild: Bool) {
        let fm = FileManager.default
        try? fm.removeItem(at: installedAgent)
        guard otherBuild else { return }
        try? fm.createDirectory(at: installedAgent.deletingLastPathComponent(), withIntermediateDirectories: true)
        let text = (try? String(contentsOf: agentScript, encoding: .utf8)) ?? ""
        try? (text + "\n# another build\n").write(to: installedAgent, atomically: true, encoding: .utf8)
        try? fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: installedAgent.path)
    }

    func put(_ name: String, _ text: String) {
        try? text.write(to: state.appendingPathComponent(name), atomically: true, encoding: .utf8)
    }

    func read(_ name: String) -> String {
        (try? String(contentsOf: state.appendingPathComponent(name), encoding: .utf8))?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    }

    func lines(_ name: String) -> [String] { read(name).split(separator: "\n").map(String.init) }

    /// The remote's state as a baseline: `grants` are other consumers' ids.
    func seed(
        running: Bool = true, sharing: Bool = false, scope: Bool = false, host: String? = nil,
        port: Int? = nil, grants: [String] = []
    ) {
        if running { put("running", "1") } else { try? FileManager.default.removeItem(at: state.appendingPathComponent("running")) }
        put("sharing", sharing ? "on" : "off")
        put("scope", scope ? "1" : "0")
        put("host", host ?? "")
        put("port", port.map(String.init) ?? "")
        put("grants", grants.map { $0 + "\n" }.joined())
        put("offers", "")
        put("pending", "")
    }

    var invocations: [[String]] {
        let count = Int(read("../count")) ?? 0
        return (1...max(count, 1)).compactMap { n in
            guard count > 0, let text = try? String(contentsOf: dir.appendingPathComponent("argv.\(n)"), encoding: .utf8)
            else { return nil }
            return text.split(separator: "\n", omittingEmptySubsequences: false).dropLast().map(String.init)
        }
    }

    // MARK: Scripts

    private static func fakeSSH(dir: String, home: String, bin: String, extraEnv: String) -> String {
        #"""
        #!/bin/sh
        D="\#(dir)"
        n=$(cat "$D/count" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$D/count"
        for a in "$@"; do printf '%s\n' "$a"; done > "$D/argv.$n"
        MODE=$(cat "$D/mode" 2>/dev/null)
        HOSTKEY="No ED25519 host key is known for box.example and you have requested strict checking."
        tunnel=0; fwd=""
        prev=""
        for a in "$@"; do
          [ "$a" = "-N" ] && tunnel=1
          [ "$prev" = "-L" ] && fwd="$a"
          prev="$a"; LAST="$a"
        done
        if [ $tunnel = 1 ]; then
          [ "$MODE" = "tunnel-fail" ] && { echo "ssh: connect to host h port 22: Connection refused" >&2; exit 255; }
          [ "$MODE" = "tunnel-hostkey" ] && { printf '%s\n' "$HOSTKEY" "Host key verification failed." >&2; exit 255; }
          LP=${fwd#127.0.0.1:}; LP=${LP%%:*}
          exec nc -l 127.0.0.1 "$LP"
        fi
        [ -e "$D/down" ] && { echo "ssh: connect to host h port 22: Connection refused" >&2; exit 255; }
        case "$MODE" in
          noisy) echo "Welcome to the fake rc";;
          flood) exec yes "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";;
          hang) exec sleep 60;;
          hostkey) printf '%s\n' "$HOSTKEY" "Host key verification failed." >&2; exit 255;;
          leak) echo "boom syrtis://secret/path?s=1 end" >&2; exit 255;;
          old2) echo "syrtis-agent: unknown argument: ctl" >&2; exit 2;;
          leak5) echo "syrtis-agent ctl: refused syrtis://secret/path?s=1" >&2; exit 5;;
        esac
        # The command takes effect, then its answer is lost (hang) or comes back as exit 6.
        case "$MODE:$LAST" in
          approve-hang:*"pending approve"*|sharing-hang:*"sharing on"*|approve-6:*"pending approve"*)
            env HOME="\#(home)" PATH="\#(bin):/usr/bin:/bin:/usr/sbin:/sbin"\#(extraEnv) /bin/sh -c "$LAST" >/dev/null 2>&1
            [ "$MODE" = approve-6 ] && { echo "syrtis-agent ctl: no valid reply came back; the command may have taken effect; run status" >&2; exit 6; }
            exec sleep 60;;
          purge-7:*"uninstall --purge"*)
            echo "syrtis-agent ctl: uninstall left objects for a manual repair: state" >&2; exit 7;;
        esac
        exec env HOME="\#(home)" PATH="\#(bin):/usr/bin:/bin:/usr/sbin:/sbin"\#(extraEnv) /bin/sh -c "$LAST"
        """#
    }

    private static func fakeAgent(state: String) -> String {
        #"""
        #!/bin/sh
        S="\#(state)"
        SELF="$0"
        frame() { printf 'SYRTIS-CTL-V1 %s\n' "$1"; }
        refuse() { echo "syrtis-agent ctl: the agent refused: invalid; nothing was changed" >&2; exit 5; }
        count() { if [ -s "$1" ]; then grep -c . "$1"; else echo 0; fi; }
        if [ "$1" = run ]; then echo 1 > "$S/running"; exit 0; fi
        [ "$1" = ctl ] || exit 2
        shift
        case "$1" in
          install|uninstall) ;;
          *) [ -e "$S/running" ] || { echo "syrtis-agent ctl: the agent is not running or not answering; nothing was changed" >&2; exit 4; } ;;
        esac
        case "$1" in
          status)
            host=$(cat "$S/host" 2>/dev/null); port=$(cat "$S/port" 2>/dev/null)
            if [ -n "$host" ]; then h="\"$host\""; else h=null; fi
            [ -n "$port" ] || port=65176
            on=false; [ "$(cat "$S/sharing" 2>/dev/null)" = on ] && on=true
            sc=false; [ "$(cat "$S/scope" 2>/dev/null)" = 1 ] && sc=true
            frame "{\"activeGrantCount\":$(count "$S/grants"),\"activeOfferCount\":$(count "$S/offers"),\"bindHost\":$h,\"bindPort\":$port,\"nodeLabel\":null,\"pendingCount\":$(count "$S/pending"),\"scopeApproved\":$sc,\"sharingEnabled\":$on}";;
          scope)
            if [ "$2" = disclosure ] && [ -e "$S/bigscope" ]; then
              frame "{\"lines\":[{\"name\":\"Home\",\"value\":\"$(printf '%0600d' 0 | tr 0 x)\"}],\"ownerInputsSha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}"
            elif [ "$2" = disclosure ]; then
              frame '{"lines":[{"name":"Home","value":"/home/fake"},{"name":"Clients","value":"claude, codex"}],"ownerInputsSha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}'
            else
              [ "$4" = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ] || refuse
              if [ "$(cat "$S/scope" 2>/dev/null)" != 1 ]; then : > "$S/grants"; fi
              echo 1 > "$S/scope"; echo approve >> "$S/log"; frame '{"scopeApproved":true}'
            fi;;
          sharing)
            if [ "$2" = on ]; then
              [ "$(cat "$S/sharing" 2>/dev/null)" = on ] && refuse
              echo on > "$S/sharing"; echo sharing-on >> "$S/log"; frame '{"sharingEnabled":true}'
            else
              echo off > "$S/sharing"; : > "$S/grants"; echo sharing-off >> "$S/log"; frame '{"sharingEnabled":false}'
            fi;;
          listen)
            echo "$3" > "$S/host"; echo "$5" > "$S/port"; echo "listen $5" >> "$S/log"
            frame "{\"bindHost\":\"$3\",\"bindPort\":$5}";;
          offer)
            if [ "$2" = create ]; then
              [ -n "$(cat "$S/host" 2>/dev/null)" ] || refuse
              k=$(( $(cat "$S/offerseq" 2>/dev/null || echo 0) + 1 )); echo $k > "$S/offerseq"
              id=$(printf '%032x' "$k"); echo "$id" >> "$S/offers"
              frame "{\"expiresAt\":1,\"offerId\":\"$id\",\"pairUrl\":\"syrtis://fake-offer?c=secret$k\"}"
            else
              grep -qx "$4" "$S/offers" 2>/dev/null || refuse
              grep -vx "$4" "$S/offers" > "$S/offers.tmp"; mv "$S/offers.tmp" "$S/offers"
              echo "offer-cancel" >> "$S/log"; frame '{"cancelled":true}'
            fi;;
          pending)
            n=$(grep -c "^$4 " "$S/pending" 2>/dev/null)
            [ "$n" = 1 ] || { echo "syrtis-agent ctl: no unique pending pairing carries that code" >&2; exit 8; }
            pid=$(grep "^$4 " "$S/pending" | cut -d' ' -f2)
            gid=$( { printf 'syrtis-pair-grant-v1\0'; printf '%s' "$pid" | xxd -r -p; } | shasum -a 256 | cut -c1-32)
            echo "$gid" >> "$S/grants"; : > "$S/offers"; grep -v "^$4 " "$S/pending" > "$S/pending.tmp"; mv "$S/pending.tmp" "$S/pending"
            frame "{\"approved\":true,\"pendingId\":\"$pid\"}";;
          grant)
            grep -qx "$4" "$S/grants" 2>/dev/null || refuse
            grep -vx "$4" "$S/grants" > "$S/grants.tmp"; mv "$S/grants.tmp" "$S/grants"
            echo "revoke" >> "$S/log"; frame '{"revoked":true}';;
          shutdown) rm -f "$S/running"; echo shutdown >> "$S/log"; frame '{"stopping":true}';;
          install)
            [ -e "$S/running" ] && exit 9
            mkdir -p "$HOME/.local/bin"
            [ "$SELF" = "$HOME/.local/bin/syrtis-agent" ] || cp "$SELF" "$HOME/.local/bin/syrtis-agent"
            echo installed >> "$S/log"; frame '{"installed":true}';;
          uninstall)
            [ -e "$S/running" ] && exit 9
            rm -f "$HOME/.local/bin/syrtis-agent"
            echo "uninstall $2" >> "$S/log"; frame '{"purged":true}';;
          *) exit 2;;
        esac
        """#
    }
}

final class SSHFakeLocal: SSHLocalPairing, @unchecked Sendable {
    private let lock = NSLock()
    let fake: SSHFake
    private(set) var rehosts: [(url: String, port: Int)] = []
    private(set) var claimUrls: [String] = []
    private(set) var removed: [String] = []
    private var pairCount = 0
    var failClaims = 0
    var fingerprint = "0123456789abcdef"
    var producerStatic = String(repeating: "ab", count: 32)
    /// The SAS the Mac shows; the producer's pending record carries `pendingSas`.
    var claimSas = "ABCD-EF01"
    var pendingSas = "ABCD-EF01"
    var onClaim: (@Sendable () -> Void)?
    var onPoll: (@Sendable () -> Void)?

    init(fake: SSHFake) { self.fake = fake }

    func rehost(pairUrl: String, localPort: Int) throws -> String {
        lock.lock()
        defer { lock.unlock() }
        rehosts.append((pairUrl, localPort))
        return "syrtis://rehost/\(localPort)"
    }

    func claim(pairUrl: String) throws -> RemoteClaim {
        lock.lock()
        if failClaims > 0 {
            failClaims -= 1
            lock.unlock()
            throw SSHRemoteError.pairing("unreachable")
        }
        claimUrls.append(pairUrl)
        pairCount += 1
        let pendingId = String(format: "%032x", 0x1000 + pairCount)
        let sas = claimSas, pending = pendingSas, fp = fingerprint
        lock.unlock()
        let old = fake.read("pending")
        fake.put("pending", old + (old.isEmpty ? "" : "\n") + "\(pending) \(pendingId)\n")
        onClaim?()
        let json = #"{"handle":7,"sas":"\#(sas)","expiresAt":0,"producerFingerprint":"\#(fp)"}"#
        return try JSONDecoder().decode(RemoteClaim.self, from: Data(json.utf8))
    }

    func poll(handle: Int64) throws -> RemotePaired {
        onPoll?()
        lock.lock()
        defer { lock.unlock() }
        let json = #"{"producerStatic":"\#(producerStatic)","fingerprint":"\#(fingerprint)","expiresAt":0}"#
        return try JSONDecoder().decode(RemotePaired.self, from: Data(json.utf8))
    }

    func peerRemove(producerStatic: String) throws {
        lock.lock()
        removed.append(producerStatic)
        lock.unlock()
    }

    func agentListenPort() -> Int? { nil }
}

final class SSHFakeTunnels: SSHTunnelControlling, @unchecked Sendable {
    private let lock = NSLock()
    private(set) var started: [SSHMachine] = []
    private(set) var stopped: [UUID] = []
    var result: SSHTunnelStatus = .up

    func start(_ machine: SSHMachine) {
        lock.lock()
        started.append(machine)
        lock.unlock()
    }

    func stop(id: UUID) {
        lock.lock()
        stopped.append(id)
        lock.unlock()
    }

    func status(id: UUID) -> SSHTunnelStatus { result }
    func waitUntilUp(id: UUID, timeout: TimeInterval) -> SSHTunnelStatus { result }
}
