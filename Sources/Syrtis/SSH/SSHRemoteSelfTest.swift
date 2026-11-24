import Darwin
import Foundation
import TokenBarCore

/// Checks for the SSH remote machine components (plan P2). Runs inside
/// `--selftest`, or alone with `--selftest-ssh`. Uses a fake `ssh` and a fake
/// agent script (`SSHRemoteTestSupport.swift`); connects to nothing.
enum SSHRemoteSelfTest {
    typealias Expect = (Bool, String) -> Void

    private final class Box<T>: @unchecked Sendable {
        var value: T?
    }

    /// Runs an async operation to its end from synchronous test code.
    static func blocking<T: Sendable>(_ op: @escaping @Sendable () async throws -> T) -> Result<T, Error> {
        let box = Box<Result<T, Error>>()
        let done = DispatchSemaphore(value: 0)
        Task.detached {
            do { box.value = .success(try await op()) } catch { box.value = .failure(error) }
            done.signal()
        }
        done.wait()
        return box.value ?? .failure(SSHRemoteError.cancelled)
    }

    static func errorOf<T>(_ result: Result<T, Error>) -> SSHRemoteError? {
        if case .failure(let error) = result { return error as? SSHRemoteError }
        return nil
    }

    static func value<T>(_ result: Result<T, Error>) -> T? {
        if case .success(let value) = result { return value }
        return nil
    }

    static func run(expect: Expect) {
        guard let fake = try? SSHFake() else {
            expect(false, "SSH setup: the fake ssh could not be created")
            return
        }
        defer { fake.cleanup() }
        inputs(expect)
        targetChecks(expect)
        frames(expect)
        runner(fake, expect)
        deploy(fake, expect)
        machines(expect)
        flows(expect)
        purgeChecks(expect)
        rollbackFlows(expect)
        manualCommandChecks(expect)
        disclosureChecks(expect)
        sshOptionChecks(fake, expect)
        supervisor(expect)
    }

    // MARK: ssh target

    private static func targetChecks(_ expect: Expect) {
        guard let bare = try? SSHTarget(host: "devbox"),
              let command = try? SSHCtlCommand.status.remoteCommand()
        else { return expect(false, "SSH target: setup") }
        let args = SSHRunner().oneShotArguments(bare, command)
        let tunnel = (try? SSHRunner().tunnelArguments(SSHMachine(
            id: UUID(), label: "m", user: nil, host: "devbox", sshPort: nil, remotePort: 47500,
            localPort: 47101, producerStatic: nil, fingerprint: nil, grantId: nil, enabled: true))) ?? []
        expect(!args.contains("-p") && !args.contains { $0.contains("@") } && args[args.count - 3] == "--"
            && args[args.count - 2] == "devbox" && !tunnel.contains("-p") && !tunnel.contains { $0.contains("@") }
            && tunnel.last == "devbox", "SSH target: empty user and port put no @ and no -p in the argv")

        let legacy = Data(#"{"id":"\#(UUID().uuidString)","label":"x","user":"me","host":"box.example","sshPort":2222,"remotePort":47500,"localPort":47101,"enabled":true}"#.utf8)
        let modern = Data(#"{"id":"\#(UUID().uuidString)","label":"x","host":"devbox","remotePort":47500,"localPort":47102,"enabled":true}"#.utf8)
        let old = try? JSONDecoder().decode(SSHMachine.self, from: legacy)
        let new = try? JSONDecoder().decode(SSHMachine.self, from: modern)
        expect(old?.user == "me" && old?.sshPort == 2222 && old?.validated() != nil
            && new?.user == nil && new?.sshPort == nil && new?.validated() != nil,
            "SSH target: a saved machine with user and port loads as before; one without them loads too")
    }

    // MARK: Inputs and frames

    private static func inputs(_ expect: Expect) {
        func rejected(_ user: String, _ host: String, _ port: Int = 22) -> Bool {
            (try? SSHTarget(user: user, host: host, port: port)) == nil
        }
        expect(!rejected("devbox", "build-1.example.com"), "SSH input: a plain user and host are accepted")
        expect(rejected("me", "-oProxyCommand=touch /tmp/x"), "SSH input: a -oProxyCommand host is refused")
        expect(rejected("me", "host%h") && rejected("me", "a b") && rejected("me", "a;b") && rejected("me", "")
            && rejected("me", "h\u{00e9}st") && rejected("me", "a\nb") && rejected("me", "-h"),
            "SSH input: a host with %, space, ;, newline, non-ASCII or a leading - is refused")
        expect(rejected("-l", "h") && rejected("me@x", "h") && rejected("", "h") && rejected("a b", "h"),
            "SSH input: a user with a leading -, @ or space is refused")
        expect(rejected("me", "h", 0) && rejected("me", "h", 70_000), "SSH input: a port outside 1-65535 is refused")
        expect(SSHInput.isDigest(String(repeating: "a", count: 64)) && !SSHInput.isDigest(String(repeating: "A", count: 64))
            && !SSHInput.isDigest(String(repeating: "a", count: 63)), "SSH input: a digest is 64 lowercase hex")
        expect(SSHInput.isSAS("ABCD-EF01") && !SSHInput.isSAS("abcd-ef01") && !SSHInput.isSAS("ABCDEF012")
            && !SSHInput.isSAS("ABCD-EF0G"), "SSH input: a SAS is XXXX-XXXX in uppercase hex")
        expect(SSHInput.isIPText("127.0.0.1") && SSHInput.isIPText("::1") && !SSHInput.isIPText("localhost")
            && !SSHInput.isIPText("1.2.3.4; rm") && !SSHInput.isIPText("1.2.3"), "SSH input: listen hosts are IP text only")
        for bad in [SSHCtlCommand.scopeApprove(digest: "x"), .pendingApprove(sas: "1'; id; '"), .offerCreate(ttl: 121),
                    .offerCreate(ttl: 0), .listen(host: "127.0.0.1", port: 80), .grantRevoke(grantId: "$(id)"),
                    .offerCancel(offerId: "../x")] {
            expect((try? bad.remoteCommand()) == nil, "SSH input: \(bad) is not rendered")
        }
        let status = try? SSHCtlCommand.status.remoteCommand().shell
        expect(status == #"sh -c '"$HOME/.local/bin/syrtis-agent" ctl status'"#,
               "SSH command: ctl is called by $HOME path inside a fixed sh -c")
        let deploy = try? SSHRemoteCommand.deploy(pin: String(repeating: "a", count: 64)).shell
        expect(deploy?.contains("mktemp -d") == true && deploy?.contains("exit 72") == true
            && deploy?.contains("\n") == false && deploy?.dropFirst(7).dropLast(1).contains("'") == false,
            "SSH command: the deploy script is one line without single quotes inside")
        expect((try? SSHRemoteCommand.deploy(pin: "abc")) == nil, "SSH command: a pin that is not 64 hex is refused")
        expect(SSHInput.redacted("fail syrtis://abc?x=1 now") == "fail syrtis://[redacted] now"
            && !SSHInput.redacted("SYRTIS://Q").contains("Q"), "SSH redaction: syrtis:// tokens are masked")
        expect(SSHAgentPin.isPlaceholder(SSHAgentPin.placeholder) && SSHInput.isDigest(SSHAgentPin.placeholder),
               "SSH pin: the placeholder is a 64-hex value that fails closed")
        do {
            // The pin is the sidecar `make remote-agent` writes beside the binary.
            let resources = FileManager.default.temporaryDirectory
                .appendingPathComponent("syrtis-pin-\(UUID().uuidString)")
            let dir = resources.appendingPathComponent("remote-agent/\(SSHAgentPin.triple)")
            defer { try? FileManager.default.removeItem(at: resources) }
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let digest = String(repeating: "ab", count: 32)
            expect(SSHAgentPin.pin(resources: resources) == SSHAgentPin.placeholder
                && !SSHAgentPin.isBundled(resources: resources), "SSH pin: no sidecar is the placeholder")
            try? Data("\(digest)  syrtis-agent\n".utf8).write(to: dir.appendingPathComponent("syrtis-agent.sha256"))
            expect(SSHAgentPin.pin(resources: resources) == digest
                && !SSHAgentPin.isBundled(resources: resources),
                "SSH pin: the sidecar's first word is the pin, but without the binary nothing is bundled")
            FileManager.default.createFile(atPath: dir.appendingPathComponent("syrtis-agent").path, contents: Data([1]))
            expect(SSHAgentPin.isBundled(resources: resources)
                && SSHAgentPin.bundledBinary(triple: "aarch64-unknown-linux-gnu", resources: resources) == nil,
                "SSH pin: binary and pin present is bundled; another triple is refused")
            try? Data("not-a-digest\n".utf8).write(to: dir.appendingPathComponent("syrtis-agent.sha256"))
            expect(SSHAgentPin.pin(resources: resources) == SSHAgentPin.placeholder, "SSH pin: a malformed sidecar is the placeholder")
        }
        // grantId: SHA-256("syrtis-pair-grant-v1\0" || pendingId bytes)[..16], checked against the shell.
        let pending = "00112233445566778899aabbccddeeff"
        let shell = Process()
        shell.executableURL = URL(fileURLWithPath: "/bin/sh")
        shell.arguments = ["-c", "{ printf 'syrtis-pair-grant-v1\\0'; printf '%s' \(pending) | xxd -r -p; } | shasum -a 256 | cut -c1-32"]
        let pipe = Pipe()
        shell.standardOutput = pipe
        try? shell.run()
        let expected = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        shell.waitUntilExit()
        expect(SSHGrantId.derive(pendingId: pending) == expected && expected.count == 32,
               "SSH grant id: derived from the raw pendingId bytes with the grant tag")
    }

    private static func frames(_ expect: Expect) {
        struct Ok: Decodable { let a: Int }
        func parse(_ text: String) -> Int? { try? SSHFrame.decode(Ok.self, from: Data(text.utf8)).a }
        expect(parse("SYRTIS-CTL-V1 {\"a\":3}\n") == 3, "SSH frame: exactly one prefixed JSON line parses")
        expect(parse("hello\nSYRTIS-CTL-V1 {\"a\":3}\n") == nil && parse("SYRTIS-CTL-V1 {\"a\":3}\nbye\n") == nil
            && parse("SYRTIS-CTL-V1 {\"a\":3}") == nil && parse("SYRTIS-CTL-V1 {\"a\":3}\r\n") == nil
            && parse("SYRTIS-CTL-V1  {\"a\":3}\n") == nil && parse("syrtis-ctl-v1 {\"a\":3}\n") == nil
            && parse("SYRTIS-CTL-V1 [1]\n") == nil && parse("") == nil,
            "SSH frame: extra lines, noise, a missing newline, CRLF or another prefix fail")
        expect((try? SSHPlatform.triple(fromUname: Data("Linux x86_64\n".utf8))) == "x86_64-unknown-linux-gnu"
            && (try? SSHPlatform.triple(fromUname: Data("../x\n".utf8))) == nil
            && (try? SSHPlatform.triple(fromUname: Data("Linux aarch64\n".utf8))) == nil
            && (try? SSHPlatform.triple(fromUname: Data("banner\nLinux x86_64\n".utf8))) == nil
            && (try? SSHPlatform.triple(fromUname: Data("Linux x86_64".utf8))) == nil,
            "SSH platform: only 'Linux x86_64' maps to a triple; ../x, other arch and noise are refused")
    }

    // MARK: Runner

    private static func runner(_ fake: SSHFake, _ expect: Expect) {
        guard let target = try? SSHTarget(user: "alice", host: "box.example", port: 2222),
              let command = try? SSHCtlCommand.status.remoteCommand()
        else { return expect(false, "SSH runner: setup") }
        let runner = fake.runner
        let args = runner.oneShotArguments(target, command)
        let fixed = SSHRunner.fixedOptions
        expect(Array(args.prefix(fixed.count)) == fixed && args.contains("ClearAllForwardings=yes"),
               "SSH runner: argv starts with the fixed options and clears forwardings")
        let separator = args.firstIndex(of: "--")
        expect(separator != nil && args[(separator ?? 0) + 1] == "alice@box.example"
            && args.last == command.shell && args.firstIndex(of: "-p").map { args[$0 + 1] == "2222" } == true
            && separator == args.count - 3, "SSH runner: -p, then -- before the destination, then the command")
        fake.seed(running: true)
        let done = (try? runner.run(target, command)).map { $0.end }
        let logged = fake.invocations.last ?? []
        expect(done == .exited(0) && logged == args && !logged.contains { $0.contains("syrtis://") },
               "SSH runner: the fake ssh sees exactly that argv, with no syrtis:// in it")

        // Output over the limit ends the child.
        fake.setMode("flood")
        let started = Date()
        let flood = try? runner.run(target, command, timeout: 20, maxStdout: 4096)
        expect(flood?.end == .outputLimit && (flood?.stdout.count ?? 0) <= 4096 && Date().timeIntervalSince(started) < 10,
               "SSH runner: stdout over the limit terminates ssh and reports outputLimit")
        fake.setMode("hang")
        let hung = Date()
        let timedOut = try? runner.run(target, command, timeout: 0.4)
        expect(timedOut?.end == .timedOut && Date().timeIntervalSince(hung) < 6,
               "SSH runner: the total timeout terminates a hung ssh")
        let cancel = SSHCancellation()
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) { cancel.cancel() }
        let cancelled = try? runner.run(target, command, timeout: 20, cancel: cancel)
        expect(cancelled?.end == .cancelled, "SSH runner: a cancel terminates ssh")

        // Error mapping and masking.
        let ctl = SSHCtl(runner: runner, target: target)
        fake.setMode("noisy")
        expect(errorOf(blockingSync { try ctl.status() }) == .malformedOutput,
               "SSH ctl: shell rc noise on stdout makes the frame parse fail")
        fake.setMode("hostkey")
        expect(errorOf(blockingSync { try ctl.status() }) == .hostKeyRejected,
               "SSH ctl: an unknown host key is reported as such")
        fake.setMode("leak")
        let leaked = errorOf(blockingSync { try ctl.status() })
        if case .unreachable(let line)? = leaked {
            expect(!line.contains("secret") && line.contains("[redacted]"), "SSH ctl: syrtis:// in stderr is masked in the error text")
        } else {
            expect(false, "SSH ctl: syrtis:// in stderr is masked in the error text")
        }
        fake.setMode("leak5")
        if case .ctl(let code, let detail)? = errorOf(blockingSync { try ctl.status() }) {
            expect(code == 5 && !detail.contains("secret"), "SSH ctl: a remote exit code keeps only the masked first stderr line")
        } else {
            expect(false, "SSH ctl: a remote exit code keeps only the masked first stderr line")
        }
        fake.setMode(nil)
        fake.seed(running: false)
        if case .ctl(let code, _)? = errorOf(blockingSync { try ctl.status() }) {
            expect(code == 4, "SSH ctl: exit 4 (agent not running) is passed through")
        } else {
            expect(false, "SSH ctl: exit 4 (agent not running) is passed through")
        }
        fake.seed(running: true)
        expect(value(blockingSync { try ctl.status() })?.sharingEnabled == false, "SSH ctl: a clean status parses")

        // Baseline: any ctl answer other than 4 (not running) or 127 (missing) means installed, so an
        // older agent without `ctl` (exit 2) is replaced by deploy instead of blocking it.
        fake.setMode("old2")
        expect(value(blockingSync { try ctl.probeAgent() }) == SSHAgentBaseline(installed: true, running: true),
               "SSH ctl: an agent answering exit 2 counts as installed")
        fake.setMode(nil)
        fake.seed(running: false)
        expect(value(blockingSync { try ctl.probeAgent() }) == SSHAgentBaseline(installed: true, running: false),
               "SSH ctl: exit 4 is installed and not running")
        fake.seed(running: true)
    }

    static func blockingSync<T>(_ op: () throws -> T) -> Result<T, Error> { Result { try op() }}

    // MARK: Deploy

    private static func deploy(_ fake: SSHFake, _ expect: Expect) {
        guard let target = try? SSHTarget(user: "alice", host: "box.example") else { return }
        var ctl = SSHCtl(runner: fake.runner, target: target)
        ctl.startGap = 0.05
        let url = fake.agentScript
        let pin = fake.agentSHA256
        try? FileManager.default.removeItem(at: fake.home.appendingPathComponent(".local/bin"))
        let before = fake.invocations.count

        // Placeholder pin: nothing runs.
        expect(errorOf(blockingSync { try ctl.deploy(binaryFor: { _ in url }, pin: SSHAgentPin.placeholder) }) == .agentPinMissing
            && fake.invocations.count == before, "SSH deploy: the placeholder pin refuses before any ssh call")

        // A group-writable ~/.local stops the flow after the one preflight call: nothing uploaded or changed.
        let dotLocal = fake.home.appendingPathComponent(".local")
        fake.seed(running: false)
        try? FileManager.default.createDirectory(at: dotLocal, withIntermediateDirectories: true)
        try? FileManager.default.setAttributes([.posixPermissions: 0o775], ofItemAtPath: dotLocal.path)
        let blocked = errorOf(blockingSync { try ctl.deploy(binaryFor: { _ in url }, pin: pin) })
        expect(blocked == .unsafeRemoteDirs(["~/.local"]) && fake.invocations.count == before + 1
            && fake.read("log").isEmpty && fake.read("running").isEmpty
            && (try? FileManager.default.contentsOfDirectory(atPath: fake.home.path))?.contains { $0.hasPrefix(".syrtis-up.") } == false
            ,"SSH deploy: a 775 ~/.local is refused after one ssh call and nothing is uploaded or changed")
        try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: dotLocal.path)
        let mark = fake.invocations.count

        // The local file does not match the pin: refused after the preflight and uname, nothing uploaded.
        let wrongPin = String(repeating: "1", count: 64)
        fake.seed(running: false)
        expect(errorOf(blockingSync { try ctl.deploy(binaryFor: { _ in url }, pin: wrongPin) }) == .integrity
            && fake.invocations.count == mark + 2 && fake.read("log").isEmpty,
            "SSH deploy: a binary that does not match the pin is not uploaded")

        // uname answers: ../x, other arch.
        let remoteUname = { (answer: String) -> Result<String, Error> in
            let script = fake.bin.appendingPathComponent("uname")
            try? "#!/bin/sh\necho '\(answer)'\n".write(to: script, atomically: true, encoding: .utf8)
            try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
            return blockingSync { try ctl.platformTriple() }
        }
        expect(errorOf(remoteUname("../x")) == .unsupportedPlatform, "SSH deploy: uname answering ../x is refused")
        expect(value(remoteUname("Linux x86_64")) == "x86_64-unknown-linux-gnu", "SSH deploy: Linux x86_64 maps through the fixed table")

        // Remote hash mismatch: the script exits 72, installs nothing, removes its temp dir.
        let leftovers = { (try? FileManager.default.contentsOfDirectory(atPath: fake.home.path))?.filter { $0.hasPrefix(".syrtis-up.") } ?? [] }
        if let command = try? SSHRemoteCommand.deploy(pin: wrongPin) {
            let result = try? fake.runner.run(target, command, stdin: .file(url), timeout: 30)
            expect(result?.end == .exited(72) && fake.read("log").isEmpty
                && !FileManager.default.fileExists(atPath: fake.home.appendingPathComponent(".local/bin/syrtis-agent").path)
                && leftovers().isEmpty, "SSH deploy: a remote sha256 mismatch exits 72, installs nothing and cleans up")
            let truncated = try? fake.runner.run(target, command, stdin: .data(Data("partial".utf8)), timeout: 30)
            expect(truncated?.end == .exited(72) && leftovers().isEmpty,
                   "SSH deploy: a truncated upload does not match and is removed")
        }

        // A binary the remote cannot execute (loader error, bad interpreter) is its own error, with the
        // first stderr line passed on and nothing left behind.
        let broken = fake.dir.appendingPathComponent("broken-agent")
        try? Data("#!/nonexistent/loader\n".utf8).write(to: broken)
        if let brokenPin = try? SSHAgentPin.sha256(of: broken) {
            if case .agentCannotRun(let detail)? = errorOf(blockingSync { try ctl.deploy(binaryFor: { _ in broken }, pin: brokenPin) }) {
                expect(detail.count <= 200 && leftovers().isEmpty,
                       "SSH deploy: a binary that cannot execute is reported as such and cleaned up")
            } else {
                expect(false, "SSH deploy: a binary that cannot execute is reported as such and cleaned up")
            }
        }

        // Fresh install, then an upgrade over a running agent: shutdown first, install, start, answer.
        let installedPath = fake.home.appendingPathComponent(".local/bin/syrtis-agent").path
        fake.seed(running: false)
        let fresh = blockingSync { try ctl.deploy(binaryFor: { _ in url }, pin: pin) }
        expect(value(fresh) != nil
            && fake.read("log").contains("installed") && FileManager.default.fileExists(atPath: installedPath)
            && fake.read("running") == "1" && leftovers().isEmpty,
            "SSH deploy: upload, hash, install, start and answer status on a clean machine")
        fake.put("log", "")
        let upgrade = blockingSync { try ctl.deploy(binaryFor: { _ in url }, pin: pin) }
        expect(value(upgrade) != nil
            && fake.lines("log") == ["shutdown", "installed"] && fake.read("running") == "1" && leftovers().isEmpty,
            "SSH deploy: an upgrade over a running agent shuts it down, reinstalls and restarts it")
    }

    // MARK: Machines store

    private static func machines(_ expect: Expect) {
        let suite = "com.tokenbar.selftest.ssh-machines"
        guard let defaults = UserDefaults(suiteName: suite) else { return }
        defaults.removePersistentDomain(forName: suite)
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = SSHMachines(defaults: defaults)
        func make(_ host: String, lport: Int, user: String = "me") -> SSHMachine {
            SSHMachine(id: UUID(), label: " \u{202E}box ", user: user, host: host, sshPort: 22, remotePort: 47500,
                       localPort: lport, producerStatic: String(repeating: "a", count: 64),
                       fingerprint: "0123456789abcdef", grantId: String(repeating: "b", count: 32), enabled: true)
        }
        let good = make("a.example", lport: 47101)
        try? store.add(good)
        typealias Row = SSHMachinesModel.Row
        let backoff = SSHTunnelStatus.backoff(attempt: 1, retryIn: 4, reason: "connection refused")
        let hostKeyBackoff = SSHTunnelStatus.backoff(attempt: 1, retryIn: 4, reason: "Host key verification failed.")
        let health = [
            SSHMachinesModel.health(Row(machine: good, tunnel: .up, needsReapproval: false)),
            SSHMachinesModel.health(Row(machine: good, tunnel: .up, needsReapproval: true)),
            SSHMachinesModel.health(Row(machine: good, tunnel: .starting, needsReapproval: false)),
            SSHMachinesModel.health(Row(machine: good, tunnel: backoff, needsReapproval: false)),
            SSHMachinesModel.health(Row(machine: good, tunnel: hostKeyBackoff, needsReapproval: false)),
            SSHMachinesModel.health(Row(machine: good, tunnel: .portInUse(47101), needsReapproval: true)),
            SSHMachinesModel.health(Row(machine: good, tunnel: .stopped, needsReapproval: false)),
        ]
        expect(health == [.ok, .warning, .warning, .warning, .error, .error, .idle],
               "SSH machines: the state dot is green for a live forward, orange for reconnecting or re-approval, red for a host key or port problem")
        expect(store.all().first?.label == "box", "SSH machines: a saved label is cleaned of bidi controls")
        expect((try? store.add(make("a.example", lport: 47102))) == nil, "SSH machines: the same user and host is not added twice")
        expect((try? store.add(make("b.example", lport: 47101))) == nil, "SSH machines: a local port is held by one machine")
        // One record that does not decode is skipped, and a store that is not a list is never overwritten.
        let lossySuite = "com.tokenbar.selftest.ssh-machines-lossy"
        if let defaults = UserDefaults(suiteName: lossySuite), let kept = try? JSONEncoder().encode(good),
           let keptText = String(data: kept, encoding: .utf8) {
            defer { defaults.removePersistentDomain(forName: lossySuite) }
            let store = SSHMachines(defaults: defaults)
            defaults.set(Data("[\(keptText),{\"id\":\"x\",\"localPort\":\"47101\"}]".utf8), forKey: SSHMachines.machinesKey)
            expect(store.all().map(\.id) == [good.id] && (try? store.add(make("c.example", lport: 47103))) != nil
                && store.all().count == 2, "SSH machines: a record that fails to decode is skipped and the others are kept")
            let broken = Data("{".utf8)
            defaults.set(broken, forKey: SSHMachines.machinesKey)
            expect((try? store.add(make("d.example", lport: 47104))) == nil && store.all().isEmpty
                && defaults.data(forKey: SSHMachines.machinesKey) == broken,
                "SSH machines: an unreadable store refuses a write and is left as it is")
        }
        // Records edited behind the app's back are dropped on load.
        var raw = store.all()
        raw.append(make("-oProxyCommand=x", lport: 47103))
        raw.append(make("c.example", lport: 47104, user: "-x"))
        raw.append(make("d.example", lport: 80))
        var badKey = make("e.example", lport: 47105)
        badKey.producerStatic = "ZZ"
        raw.append(badKey)
        defaults.set(try? JSONEncoder().encode(raw), forKey: SSHMachines.machinesKey)
        expect(store.all().map(\.host) == ["a.example"], "SSH machines: load drops a bad host, user, port or key")

        // Ports.
        let taken = store.all().map(\.localPort)
        var seen: Set<Int> = []
        for _ in 0..<50 {
            if let port = try? store.allocateLocalPort(avoiding: [47200], canBind: { $0 != 47300 }) { seen.insert(port) }
        }
        expect(!seen.isEmpty && seen.allSatisfy { SSHInput.portRange.contains($0) && !taken.contains($0) && $0 != 47200 && $0 != 47300 },
               "SSH ports: lport avoids saved ports, the agent's port and ports that do not bind")
        expect((try? store.allocateLocalPort(canBind: { _ in false })) == nil, "SSH ports: no bindable port is reported, not guessed")

        // Re-approval is needed when the peer is gone or not active.
        let peerJSON = { (state: String) in
            #"[{"producerStatic":"\#(String(repeating: "a", count: 64))","fingerprint":"0123456789abcdef","nodeLabel":null,"host":"127.0.0.1","port":47101,"createdAt":0,"expiresAt":0,"state":"\#(state)"}]"#
        }
        func peers(_ state: String) -> [RemotePeer] { (try? JSONDecoder().decode([RemotePeer].self, from: Data(peerJSON(state).utf8))) ?? [] }
        expect(!SSHMachines.needsReapproval(good, peers: peers("active")) && SSHMachines.needsReapproval(good, peers: peers("revoked"))
            && SSHMachines.needsReapproval(good, peers: []), "SSH machines: a revoked or missing peer needs re-approval")

        // SSH peers are named by the machine and left out of "linked devices".
        let otherJSON = #"[{"producerStatic":"\#(String(repeating: "c", count: 64))","fingerprint":"fedcba9876543210","nodeLabel":"lan box","host":"192.168.1.5","port":47000,"createdAt":0,"expiresAt":0,"state":"active"}]"#
        let other = (try? JSONDecoder().decode([RemotePeer].self, from: Data(otherJSON.utf8))) ?? []
        let mixed = peers("active") + other
        let linkedOnly = SSHPeerNames.linked(mixed, machines: [good])
        expect(linkedOnly.map(\.fingerprint) == ["fedcba9876543210"]
            && SSHPeerNames.linked(mixed, machines: []).count == 2,
               "SSH peers: a saved machine's peer is left out of linked devices, others stay")
        var byFingerprintOnly = good
        byFingerprintOnly.producerStatic = nil
        expect(SSHPeerNames.linked(peers("active"), machines: [byFingerprintOnly]).isEmpty,
               "SSH peers: a machine matched by fingerprint alone is also left out")
        let overlaid = SSHPeerNames.overlaid(
            ["0123456789abcdef": "127.0.0.1:47101", "fedcba9876543210": "lan box"], machines: [good], peers: mixed)
        expect(overlaid["0123456789abcdef"] == good.label && overlaid["fedcba9876543210"] == "lan box"
            && !overlaid.values.contains { $0.contains("127.0.0.1") },
               "SSH peers: the machine label replaces the tunnel address, other names stay")
    }

    // MARK: Flows

    final class Rig: @unchecked Sendable {
        let fake: SSHFake
        let local: SSHFakeLocal
        let tunnels = SSHFakeTunnels()
        let store: SSHMachines
        let suite: String
        let coordinator: SSHMachineCoordinator

        init?(callTimeout: TimeInterval? = nil) {
            guard let fake = try? SSHFake() else { return nil }
            self.fake = fake
            local = SSHFakeLocal(fake: fake)
            suite = "com.tokenbar.selftest.ssh-flow-\(UUID().uuidString)"
            guard let defaults = UserDefaults(suiteName: suite) else { return nil }
            store = SSHMachines(defaults: defaults)
            let runner = fake.runner
            let agent = fake.uploaded
            var deps = SSHMachineCoordinator.Dependencies(
                store: store, tunnels: tunnels, local: local,
                makeRemote: { target, cancel in
                    var ctl = SSHCtl(runner: runner, target: target, cancel: cancel)
                    ctl.startGap = 0.05
                    if let callTimeout { ctl.callTimeout = callTimeout }
                    return ctl
                },
                binaryFor: { _ in agent })
            deps.pin = fake.agentSHA256
            deps.randomRemotePort = { Int.random(in: 47500...47599) }
            coordinator = SSHMachineCoordinator(deps)
        }

        func cleanup() {
            UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
            fake.cleanup()
        }
    }

    final class Seen: @unchecked Sendable {
        var request: SSHConsentRequest?
    }

    static let other = String(repeating: "c", count: 32)

    private static func flows(_ expect: Expect) {
        // Add, clean remote.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: false)
            let seen = Seen()
            let result = blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: "Box", confirm: { request in
                    seen.request = request
                    return true
                })
            }
            let machine = value(result)
            let grants = rig.fake.lines("grants")
            expect(machine != nil && machine?.grantId.map { grants == [$0] } == true
                && machine?.producerStatic == rig.local.producerStatic && machine?.fingerprint == rig.local.fingerprint
                && SSHInput.portRange.contains(machine?.remotePort ?? 0) && SSHInput.portRange.contains(machine?.localPort ?? 0)
                && rig.store.all() == (machine.map { [$0] } ?? []),
                "SSH add: deploys, pairs, derives the grant id and saves the machine (\(String(describing: errorOf(result))))")
            expect(seen.request?.baseline.sharingEnabled == false && seen.request?.disclosure.lines.count == 2
                && seen.request?.endsOtherGrants == false
                && rig.fake.read("scope") == "1" && rig.fake.read("sharing") == "on"
                && rig.fake.read("host") == "127.0.0.1" && rig.fake.read("port") == String(machine?.remotePort ?? 0),
                "SSH add: the consent request carries the baseline and disclosure; scope, sharing and listen follow the yes")
            expect(rig.local.rehosts.count == 1 && rig.local.rehosts[0].port == machine?.localPort
                && rig.tunnels.started.last?.remotePort == machine?.remotePort,
                "SSH add: the Pair URL is rehosted to the machine's lport and the tunnel targets its rport")
            let argvs = rig.fake.invocations.flatMap { $0 }
            expect(!argvs.contains { $0.contains("syrtis://") || $0.contains("secret") }
                && rig.fake.invocations.allSatisfy { $0.contains("--") && $0.contains("ClearAllForwardings=yes") },
                "SSH add: no ssh argv carries the Pair URL, and every call has -- and the fixed options")
        } else { expect(false, "SSH flows: rig") }

        // Decline before any change.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: false, scope: false)
            let result = blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: nil, confirm: { _ in false })
            }
            expect(errorOf(result) == .declined && rig.fake.read("sharing") == "off" && rig.fake.read("scope") == "0"
                && rig.fake.read("host").isEmpty && rig.fake.lines("grants").isEmpty && rig.store.all().isEmpty
                && !rig.fake.read("log").contains("approve") && rig.tunnels.started.isEmpty,
                "SSH add: declining changes nothing on the remote and saves nothing")
        }

        // Approving an unapproved scope ends other grants: the request says so.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: false, grants: [other])
            let seen = Seen()
            _ = blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: nil, confirm: { request in
                    seen.request = request
                    return false
                })
            }
            expect(seen.request?.endsOtherGrants == true, "SSH add: the consent request warns that approving ends other grants")
        }

        // rport retry: claim fails twice, a new rport each time, old offers cancelled.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true)
            rig.local.failClaims = 2
            let result = blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: nil, confirm: { _ in true })
            }
            let listens = rig.fake.lines("log").filter { $0.hasPrefix("listen ") }
            expect(value(result) != nil && listens.count == 3 && Set(listens).count == 3
                && rig.local.rehosts.count == 3 && value(result)?.remotePort == Int(listens.last?.dropFirst(7) ?? "0"),
                "SSH add: a failed claim retries on a different rport and the last one is saved (\(listens.count) listens)")
            expect(rig.fake.lines("offers").isEmpty && rig.fake.lines("log").filter { $0 == "offer-cancel" }.count == 2,
                   "SSH add: the offers of the two failed attempts are cancelled; the last is consumed by the approval")
        }

        // Remove with another consumer, then alone; reapprove; disconnected remove.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777, grants: [other])
            let machine = value(blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: nil, confirm: { _ in true })
            })
            guard let machine, let grantId = machine.grantId else { return expect(false, "SSH remove: setup") }
            expect(Set(rig.fake.lines("grants")) == [other, grantId], "SSH remove: both grants active before")
            let removed = value(blocking { [coordinator = rig.coordinator] in try await coordinator.remove(id: machine.id) })
            expect(removed?.manualCommands.isEmpty == true && rig.fake.lines("grants") == [other] && rig.fake.read("sharing") == "on"
                && rig.store.all().isEmpty && rig.tunnels.stopped.contains(machine.id)
                && rig.local.removed == [machine.producerStatic ?? ""],
                "SSH remove: only this receiver's grant is revoked; the other stays active and sharing stays on")

            // Alone: sharing goes off once nothing is left.
            rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777)
            let second = value(blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box2.example", label: nil, confirm: { _ in true })
            })
            let gone = second.flatMap { m in value(blocking { [coordinator = rig.coordinator] in try await coordinator.remove(id: m.id) }) }
            expect(gone?.manualCommands.isEmpty == true && rig.fake.read("sharing") == "off" && rig.fake.lines("grants").isEmpty,
                   "SSH remove: sharing is turned off only when the last grant is gone")

            // Disconnected: the machine is removed here anyway and the commands to run by hand come back.
            rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777, grants: [other])
            let third = value(blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box3.example", label: nil, confirm: { _ in true })
            })
            rig.fake.setDown(true)
            let offline = third.flatMap { m in value(blocking { [coordinator = rig.coordinator] in try await coordinator.remove(id: m.id) }) }
            expect(offline?.manualCommands.count == 1 && offline?.error != nil && rig.store.all().isEmpty
                && offline?.manualCommands.first?.contains("grant revoke --grant-id \(third?.grantId ?? "?")") == true
                && rig.fake.lines("grants").count == 2,
                "SSH remove: an unreachable remote still removes the machine here and returns the revoke command to run by hand")

            // With purge, the destructive commands come after a status and a condition on it.
            rig.fake.setDown(false)
            let fourth = value(blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box4.example", label: nil, confirm: { _ in true })
            })
            rig.fake.setDown(true)
            let purged = fourth.flatMap { m in
                value(blocking { [coordinator = rig.coordinator] in try await coordinator.remove(id: m.id, purge: true) })
            }
            let owed = purged?.manualCommands ?? []
            func at(_ text: String) -> Int { owed.firstIndex { $0.contains(text) } ?? -1 }
            expect(at("ctl status") > at("grant revoke") && at("activeGrantCount is 0") > at("ctl status")
                && at("ctl shutdown") > at("activeGrantCount is 0") && at("--purge") > at("ctl shutdown") && at("grant revoke") >= 0,
                "SSH remove: when the remote is unreachable, purge commands follow a status and a condition on it")
        }

        // Re-approval.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777)
            guard let machine = value(blocking({ [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: nil, confirm: { _ in true })
            })) else { return expect(false, "SSH reapprove: setup") }
            rig.fake.put("grants", "") // someone else's approval or a sharing toggle ended ours
            let again = value(blocking { [coordinator = rig.coordinator] in
                try await coordinator.reapprove(id: machine.id, confirm: { _ in true }, confirmIdentityChange: { _, _ in false })
            })
            expect(again?.machine.localPort == machine.localPort && again?.identityChanged == false
                && again?.machine.grantId != machine.grantId && rig.fake.lines("grants") == [again?.machine.grantId ?? "?"]
                && rig.store.all().first?.grantId == again?.machine.grantId,
                "SSH reapprove: the same lport, a fresh grant, no identity change")

            // A changed producer key: declined rolls back and keeps the record; accepted is reported.
            rig.fake.put("grants", "")
            rig.fake.put("scope", "0")
            rig.local.fingerprint = "fedcba9876543210"
            rig.local.producerStatic = String(repeating: "cd", count: 32)
            let declined = blocking { [coordinator = rig.coordinator] in
                try await coordinator.reapprove(id: machine.id, confirm: { _ in true }, confirmIdentityChange: { _, _ in false })
            }
            expect(errorOf(declined) == .identityChanged && rig.fake.read("scope") == "0"
                && rig.fake.lines("grants").isEmpty && rig.fake.lines("offers").isEmpty
                && rig.store.all().first?.fingerprint == again?.machine.fingerprint,
                "SSH reapprove: a changed responderStatic that is not accepted approves neither the scope nor a grant and keeps the record")
            rig.fake.put("pending", "") // the declined attempt's pending record is not this one
            let accepted = value(blocking { [coordinator = rig.coordinator] in
                try await coordinator.reapprove(id: machine.id, confirm: { _ in true }, confirmIdentityChange: { _, _ in true })
            })
            expect(accepted?.identityChanged == true && rig.store.all().first?.fingerprint == "fedcba9876543210"
                && rig.local.removed.contains(machine.producerStatic ?? "?"),
                "SSH reapprove: an accepted new key is reported and the old peer is removed")
        }
    }

    // MARK: Supervisor

    private static func supervisor(_ expect: Expect) {
        guard let fake = try? SSHFake() else { return }
        defer { fake.cleanup() }
        let lport = (try? SSHMachines(defaults: UserDefaults(suiteName: "com.tokenbar.selftest.ssh-ports")!)
            .allocateLocalPort()) ?? 47150
        let machine = SSHMachine(
            id: UUID(), label: "m", user: "me", host: "box.example", sshPort: 2200, remotePort: 47500,
            localPort: lport, producerStatic: nil, fingerprint: nil, grantId: nil, enabled: true)
        let state = fake.dir.appendingPathComponent("tunnels")
        var options = SSHTunnelSupervisor.Options(runner: fake.runner, stateDirectory: state)
        options.backoff = [0.1, 0.2]
        options.pollInterval = 0.05
        let supervisor = SSHTunnelSupervisor(options: options)
        defer { supervisor.stopAll() }

        // argv: the fixed options, the forward spec, no ClearAllForwardings.
        let args = (try? fake.runner.tunnelArguments(machine)) ?? []
        expect(Array(args.prefix(SSHRunner.fixedOptions.count)) == SSHRunner.fixedOptions
            && !args.contains("ClearAllForwardings=yes") && args.contains("ExitOnForwardFailure=yes")
            && args.contains("ServerAliveInterval=30") && args.contains("-N")
            && args.suffix(4) == ["-L", "127.0.0.1:\(lport):127.0.0.1:47500", "--", "me@box.example"]
            && args.firstIndex(of: "-p").map { args[$0 + 1] == "2200" } == true,
            "SSH tunnel: -N -L 127.0.0.1:lport:127.0.0.1:rport with ExitOnForwardFailure and -- before the destination")

        // Up, drop, reconnect.
        supervisor.start(machine)
        let up = supervisor.waitUntilUp(id: machine.id, timeout: 10)
        expect(up == .up && !SSHPorts.canBindLoopback(lport), "SSH tunnel: the forward is up and holds the local port")
        let firstPid = readPid(state, machine.id)
        if let pid = firstPid { kill(pid, SIGKILL) }
        var sawDrop = false
        var secondPid: Int32?
        for _ in 0..<100 {
            Thread.sleep(forTimeInterval: 0.1)
            if case .backoff = supervisor.status(id: machine.id) { sawDrop = true }
            if let pid = readPid(state, machine.id), pid != firstPid, supervisor.status(id: machine.id) == .up { secondPid = pid; break }
        }
        expect(firstPid != nil && secondPid != nil, "SSH tunnel: after the ssh process dies the supervisor reconnects (backoff seen: \(sawDrop))")
        supervisor.stop(id: machine.id)
        expect(supervisor.status(id: machine.id) == .stopped && SSHPorts.canBindLoopback(lport)
            && readPid(state, machine.id) == nil, "SSH tunnel: stop ends ssh, frees the port and removes the pid file")

        // A busy port is reported, never changed; ssh is not started.
        let held = HeldPort()
        let busy = SSHMachine(
            id: UUID(), label: "b", user: "me", host: "box.example", sshPort: 22, remotePort: 47500,
            localPort: held.port, producerStatic: nil, fingerprint: nil, grantId: nil, enabled: true)
        let before = fake.invocations.count
        supervisor.start(busy)
        let state1 = supervisor.waitUntilUp(id: busy.id, timeout: 5)
        expect(state1 == .portInUse(held.port) && fake.invocations.count == before,
               "SSH tunnel: a busy local port is reported as portInUse and ssh is not started")
        held.release()
        var later = supervisor.status(id: busy.id)
        for _ in 0..<200 where later != .up {
            Thread.sleep(forTimeInterval: 0.05)
            later = supervisor.status(id: busy.id)
        }
        expect(later == .up, "SSH tunnel: once the port is free the same port comes up")
        supervisor.stop(id: busy.id)

        // Stale processes: only the same uid with the exact argv are ended.
        reapChecks(fake, expect)
    }

    private final class HeldPort: @unchecked Sendable {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        private(set) var port = 0

        init() {
            var address = sockaddr_in()
            address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
            address.sin_family = sa_family_t(AF_INET)
            address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
            for candidate in SSHInput.portRange.shuffled() {
                address.sin_port = in_port_t(UInt16(candidate)).bigEndian
                let ok = withUnsafePointer(to: &address) {
                    $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
                }
                if ok == 0 { port = candidate; break }
            }
            listen(fd, 1)
        }

        func release() { close(fd) }
    }

    private static func readPid(_ dir: URL, _ id: UUID) -> Int32? {
        guard let data = try? Data(contentsOf: dir.appendingPathComponent("\(id.uuidString).pid")) else { return nil }
        return (try? JSONDecoder().decode(SSHTunnelSupervisor.PidRecord.self, from: data))?.pid
    }

    private static func reapChecks(_ fake: SSHFake, _ expect: Expect) {
        let state = fake.dir.appendingPathComponent("stale")
        try? FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
        var runner = SSHRunner()
        runner.sshPath = "/usr/bin/yes"
        var options = SSHTunnelSupervisor.Options(runner: runner, stateDirectory: state)
        options.pollInterval = 0.05
        let supervisor = SSHTunnelSupervisor(options: options)
        let machine = SSHMachine(
            id: UUID(), label: "s", user: "me", host: "box.example", sshPort: 22, remotePort: 47500,
            localPort: 47190, producerStatic: nil, fingerprint: nil, grantId: nil, enabled: true)
        func spawn(_ args: [String]) -> Process {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/yes")
            process.arguments = args
            process.standardOutput = FileHandle.nullDevice
            try? process.run()
            return process
        }
        func record(_ pid: Int32, as machine: SSHMachine, name: String) {
            let data = try? JSONEncoder().encode(SSHTunnelSupervisor.PidRecord(pid: pid, machine: machine))
            try? data?.write(to: state.appendingPathComponent("\(name).pid"))
        }
        let exact = (try? runner.tunnelArguments(machine)) ?? []
        let match = spawn(exact)
        let wrongArgv = spawn(exact + ["extra"])
        let otherUid = spawn(exact)
        Thread.sleep(forTimeInterval: 0.3)
        record(match.processIdentifier, as: machine, name: "match")
        record(wrongArgv.processIdentifier, as: machine, name: "wrongargv")
        record(otherUid.processIdentifier, as: machine, name: "uid")
        // The uid check: run the reaper as if this were another uid, on its own file only.
        let onlyUid = fake.dir.appendingPathComponent("stale-uid")
        try? FileManager.default.createDirectory(at: onlyUid, withIntermediateDirectories: true)
        try? FileManager.default.moveItem(at: state.appendingPathComponent("uid.pid"), to: onlyUid.appendingPathComponent("uid.pid"))
        var uidOptions = options
        uidOptions.stateDirectory = onlyUid
        let wrongUid = SSHTunnelSupervisor(options: uidOptions).reapStale(uid: getuid() &+ 1)
        expect(wrongUid.isEmpty && otherUid.isRunning, "SSH stale: a process of another uid is left alone")
        let ended = supervisor.reapStale()
        Thread.sleep(forTimeInterval: 0.3)
        expect(ended == [match.processIdentifier] && !match.isRunning && wrongArgv.isRunning,
               "SSH stale: only the process whose full argv is the tunnel's is ended; a different argv is left alone")
        expect((try? FileManager.default.contentsOfDirectory(atPath: state.path))?.isEmpty == true,
               "SSH stale: pid files are removed whether or not a process was ended")
        wrongArgv.terminate()
        otherUid.terminate()
    }

    // MARK: Disclosure

    static func disclosureChecks(_ expect: Expect) {
        func decode(_ lines: [(String, String)]) -> SSHDisclosure? {
            let items = lines.map { #"{"name":"\#($0.0)","value":"\#($0.1)"}"# }.joined(separator: ",")
            let json = #"{"lines":[\#(items)],"ownerInputsSha256":"\#(String(repeating: "a", count: 64))"}"#
            return try? SSHDisclosure(decoding: Data("SYRTIS-CTL-V1 \(json)\n".utf8))
        }
        let ok = decode([("Home", "/home/x")])
        let manyLines = decode((0..<(SSHDisclosure.maxLines + 1)).map { ("n\($0)", "v") })
        let exactLines = decode((0..<SSHDisclosure.maxLines).map { ("n\($0)", "v") })
        let longValue = decode([("Home", String(repeating: "x", count: SSHDisclosure.maxValue + 1))])
        let exactValue = decode([("Home", String(repeating: "x", count: SSHDisclosure.maxValue))])
        let tabbed = decode([("Home", "/a\\tb")])
        let longName = decode([(String(repeating: "n", count: SSHDisclosure.maxName + 1), "v")])
        expect(ok?.truncated == false && exactLines?.truncated == false && exactValue?.truncated == false
            && manyLines?.truncated == true && manyLines?.lines.count == SSHDisclosure.maxLines
            && longValue?.truncated == true && longValue?.lines.first?.value.count == SSHDisclosure.maxValue
            && longName?.truncated == true && tabbed?.truncated == true,
            "SSH disclosure: more than 64 lines, a name over 64 or a value over 512 characters, or text changed by cleaning, carries the truncated flag; the limits themselves do not")

        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: false, scope: false)
            rig.fake.put("bigscope", "1")
            let seen = Seen()
            let result = blocking { [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: "box.example", label: nil, confirm: { request in
                    seen.request = request
                    return true
                })
            }
            expect(errorOf(result) == .disclosureTruncated && seen.request?.canApprove == false
                && seen.request?.disclosure.truncated == true && rig.fake.read("scope") == "0"
                && rig.fake.read("sharing") == "off" && !rig.fake.lines("log").contains { $0.contains("approve") },
                "SSH add: a cut disclosure cannot be approved even when the confirmation says yes, and nothing is approved")
        }
    }

    // MARK: ssh options

    static func sshOptionChecks(_ fake: SSHFake, _ expect: Expect) {
        let machine = SSHMachine(
            id: UUID(), label: "m", user: "alice", host: "box.example", sshPort: nil, remotePort: 47500,
            localPort: 47101, producerStatic: nil, fingerprint: nil, grantId: nil, enabled: true)
        guard let target = try? SSHTarget(user: "alice", host: "box.example"),
              let command = try? SSHCtlCommand.status.remoteCommand(),
              let tunnel = try? SSHRunner().tunnelArguments(machine)
        else { return expect(false, "SSH options: setup") }
        let oneShot = SSHRunner().oneShotArguments(target, command)
        func hasOption(_ args: [String], _ option: String) -> Bool {
            zip(args, args.dropFirst()).contains { $0 == "-o" && $1 == option }
        }
        let fixed = ["NoHostAuthenticationForLocalhost=no", "ForkAfterAuthentication=no", "RemoteCommand=none"]
        expect(fixed.allSatisfy { hasOption(oneShot, $0) && hasOption(tunnel, $0) }
            && hasOption(oneShot, "SessionType=default") && !hasOption(oneShot, "SessionType=none")
            && hasOption(tunnel, "SessionType=none") && !hasOption(tunnel, "SessionType=default")
            && Array(oneShot.prefix(SSHRunner.fixedOptions.count)) == SSHRunner.fixedOptions
            && Array(tunnel.prefix(SSHRunner.fixedOptions.count)) == SSHRunner.fixedOptions
            && tunnel.contains("-N") && !hasOption(tunnel, "ClearAllForwardings=yes")
            && hasOption(oneShot, "StdinNull=no") && !hasOption(tunnel, "StdinNull=no"),
            "SSH options: every call pins NoHostAuthenticationForLocalhost, ForkAfterAuthentication and RemoteCommand; a tunnel has SessionType=none, a command SessionType=default")

        // The ssh that ships with macOS must accept every one of them.
        guard FileManager.default.isExecutableFile(atPath: "/usr/bin/ssh") else { return }
        for (name, args) in [("command", oneShot), ("tunnel", tunnel)] {
            let probe = ["-G", "-F", "/dev/null"] + args.filter { $0 != command.shell }
            let result = try? SSHRunner.execute(
                path: "/usr/bin/ssh", arguments: probe, environment: SSHRunner().environment(), stdin: .none,
                timeout: 10, maxStdout: 64 * 1024, cancel: nil)
            expect(result?.end == .exited(0), "SSH options: /usr/bin/ssh -G accepts the \(name) options")
        }
    }
}
