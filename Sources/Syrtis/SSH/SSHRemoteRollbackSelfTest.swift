import Darwin
import Foundation
import TokenBarCore

/// Selftests for the undo a failed or cancelled flow does in the same flow, the
/// commands shown when it cannot, and the purge rule. Fake ssh only.
extension SSHRemoteSelfTest {
    private static let host = "box.example"

    private static func add(
        _ rig: Rig, confirm: @escaping SSHConsent = { _ in true }, cancel: SSHCancellation = SSHCancellation(),
        host: String = Self.host
    ) -> Result<SSHMachine, Error> {
        blocking { [coordinator = rig.coordinator] in
            try await coordinator.add(user: "me", host: host, label: nil, confirm: confirm, cancel: cancel)
        }
    }

    private static func incomplete(_ result: Result<SSHMachine, Error>) -> SSHRollbackIncomplete? {
        if case .failure(let error) = result { return error as? SSHRollbackIncomplete }
        return nil
    }

    // MARK: Purge

    static func purgeChecks(_ expect: Expect) {
        guard let rig = Rig() else { return expect(false, "SSH purge: rig") }
        defer { rig.cleanup() }
        func purge(at address: String) -> SSHRemoveResult? {
            guard let machine = value(blocking({ [coordinator = rig.coordinator] in
                try await coordinator.add(user: "me", host: address, label: nil, confirm: { _ in true })
            })) else { return nil }
            rig.fake.put("log", "")
            return value(blocking { [coordinator = rig.coordinator] in try await coordinator.remove(id: machine.id, purge: true) })
        }
        rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777, grants: [other])
        let shared = purge(at: host)
        expect(shared?.purgeSkippedGrants == 1 && !rig.fake.lines("log").contains { $0.hasPrefix("uninstall") }
            && !rig.fake.lines("log").contains("shutdown") && rig.fake.lines("grants") == [other] && rig.fake.read("running") == "1",
            "SSH purge: with another device's grant on the machine nothing is shut down or uninstalled, and the owner is told")
        rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777)
        let alone = purge(at: "box2.example")
        expect(alone?.purgeSkippedGrants == nil && rig.fake.lines("log").contains("uninstall --purge")
            && rig.fake.lines("log").contains("shutdown"),
            "SSH purge: once our own grant is revoked and none is left the agent is shut down and uninstalled with --purge")

        // The purge itself exits 7: its error and detail are kept, and rerunning it is the one command left.
        rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777)
        rig.fake.setMode("purge-7")
        let leftover = purge(at: "box3.example")
        rig.fake.setMode(nil)
        let detail = "syrtis-agent ctl: uninstall left objects for a manual repair: state"
        let rerun = (try? SSHTarget(user: "me", host: "box3.example")).map { SSHManualCommands.lines([.uninstall(purge: true)], on: $0) }
        expect(leftover?.error == .ctl(code: 7, detail: detail) && leftover?.manualCommands == rerun && rerun?.count == 1
            && !rig.store.all().contains { $0.host == "box3.example" }
            && leftover.flatMap(SSHMachinesModel.removeNotice)?.contains(detail) == true,
            "SSH purge: an uninstall that exits 7 keeps its detail, lists only the purge to rerun, removes the machine here, and the notice shows the detail")
    }

    // MARK: Undo in the same flow

    static func rollbackFlows(_ expect: Expect) {
        // A decline on a clean machine uninstalls (no purge) the agent this flow installed.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.setInstalled(otherBuild: false)
            rig.fake.seed(running: false)
            let result = add(rig, confirm: { _ in false })
            let log = rig.fake.lines("log")
            expect(errorOf(result) == .declined && log.contains("installed") && log.contains("shutdown")
                && log.contains { $0.hasPrefix("uninstall") && !$0.contains("purge") }
                && !FileManager.default.fileExists(atPath: rig.fake.installedAgent.path) && rig.store.all().isEmpty,
                "SSH rollback: declining on a clean machine shuts down and uninstalls (no purge) the agent this flow installed")
        } else { expect(false, "SSH rollback: rig") }

        // An agent that was installed and off is stopped again; one that was running keeps running.
        for wasRunning in [false, true] {
            guard let rig = Rig() else { continue }
            defer { rig.cleanup() }
            rig.fake.seed(running: wasRunning)
            let result = add(rig, confirm: { _ in false })
            expect(errorOf(result) == .declined && (rig.fake.read("running") == "1") == wasRunning
                && FileManager.default.fileExists(atPath: rig.fake.installedAgent.path)
                && !rig.fake.lines("log").contains { $0.hasPrefix("uninstall") },
                "SSH rollback: an installed agent is put back \(wasRunning ? "running" : "stopped"), never uninstalled")
        }

        // A failure after the yes (no pending carries the SAS), with another consumer's grant and sharing on: only our own parts go.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777, grants: [other])
            rig.local.pendingSas = "0000-1111"
            let result = add(rig)
            if case .ctl(let code, _)? = errorOf(result) {
                expect(code == 8 && rig.fake.lines("grants") == [other] && rig.fake.read("sharing") == "on"
                    && rig.fake.lines("offers").isEmpty && rig.store.all().isEmpty && !rig.tunnels.stopped.isEmpty
                    && rig.tunnels.started.last?.remotePort == 47777,
                    "SSH rollback: a refused pairing cancels our offer and stops the tunnel; the other grant, sharing and listener stay (its port is reused)")
            } else { expect(false, "SSH rollback: a refused pairing is exit 8 (\(String(describing: errorOf(result))))") }
        }

        // The machine is lost after the claim: the first failed step ends the tries, and everything left is shown as commands.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.setInstalled(otherBuild: false)
            rig.fake.seed(running: false)
            let cancel = SSHCancellation()
            rig.local.onClaim = { [fake = rig.fake] in
                fake.setDown(true)
                cancel.cancel()
            }
            let result = add(rig, cancel: cancel)
            let owed = incomplete(result)
            let text = (owed?.commands ?? []).joined(separator: "\n")
            expect(owed != nil && (owed?.cause as? SSHRemoteError) == .cancelled
                && text.contains("sharing off") && text.contains("ctl shutdown") && text.contains("ctl uninstall")
                && !text.contains("--purge") && !text.contains("grant revoke") && rig.store.all().isEmpty,
                "SSH rollback: when the machine is lost, the error carries the sharing off, shutdown and uninstall commands")
        }

        // Cancelled while the poll waits: nothing is saved, and the grant and the peer are undone.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: false, scope: false)
            let cancel = SSHCancellation()
            rig.local.onPoll = { cancel.cancel() }
            let result = add(rig, cancel: cancel)
            expect(errorOf(result) == .cancelled && rig.store.all().isEmpty && rig.fake.lines("grants").isEmpty
                && rig.local.removed.contains(rig.local.producerStatic),
                "SSH rollback: a cancel during the poll is not committed; the grant is revoked and the peer removed")
        }

        // A re-approval cancelled during the poll: a replacement peer is removed, the original peer and forward stay.
        for changed in [true, false] {
            guard let rig = Rig() else { continue }
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: true)
            guard let original = value(add(rig)), let originalKey = original.producerStatic else {
                expect(false, "SSH rollback: re-approve cancel setup")
                continue
            }
            rig.fake.put("grants", "")
            rig.local.removeAllRemoved()
            let replacement = String(repeating: "cd", count: 32)
            if changed {
                rig.local.producerStatic = replacement
                rig.local.fingerprint = "fedcba9876543210"
            }
            let cancel = SSHCancellation()
            rig.local.onPoll = { cancel.cancel() }
            let result = blocking { [coordinator = rig.coordinator] in
                try await coordinator.reapprove(
                    id: original.id, confirm: { _ in true }, confirmIdentityChange: { _, _ in true }, cancel: cancel)
            }
            let removed = rig.local.removed
            expect(errorOf(result) == .cancelled && rig.store.all() == [original] && rig.fake.lines("grants").isEmpty
                && removed.contains(replacement) == changed && !removed.contains(originalKey)
                && rig.tunnels.started.last == original && !rig.tunnels.stopped.contains(original.id),
                "SSH rollback: a cancelled re-approval with \(changed ? "a changed" : "the same") producer keeps the original peer and forward\(changed ? " and removes the replacement" : "")")
        }

        // The approval's answer is lost. Sharing was off: the rollback's sharing off ends the orphan. Sharing was on: it is shown for the owner.
        for sharing in [false, true] {
            guard let rig = Rig(callTimeout: 3) else { continue }
            defer { rig.cleanup() }
            if sharing {
                rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777, grants: [other])
            } else {
                rig.fake.seed(running: true, sharing: false, scope: false)
            }
            rig.fake.setMode("approve-hang")
            let result = add(rig)
            rig.fake.setMode(nil)
            let text = (incomplete(result)?.commands ?? []).joined(separator: "\n")
            expect(sharing
                ? (text.contains("ctl status") && text.contains("sharing off") && rig.fake.lines("grants").count == 2)
                : (incomplete(result) == nil && errorOf(result) == .timedOut && rig.fake.lines("grants").isEmpty),
                "SSH rollback: a lost approval with sharing \(sharing ? "on is handed to the owner as commands" : "off is ended by sharing off")")
        }

        // The approval exits 6 (no valid reply) after the grant was added: it may have gone through.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: true, host: "127.0.0.1", port: 47777, grants: [other])
            rig.fake.setMode("approve-6")
            let result = add(rig)
            rig.fake.setMode(nil)
            let text = (incomplete(result)?.commands ?? []).joined(separator: "\n")
            var cause6 = false
            if case .ctl(6, _)? = incomplete(result)?.cause as? SSHRemoteError { cause6 = true }
            expect(cause6 && text.contains("ctl status") && text.contains("sharing off") && rig.fake.lines("grants").count == 2,
                "SSH rollback: an approval answered with exit 6 may have gone through, so status and sharing off are handed to the owner")
        }

        // `sharing on` refused (someone turned it on after the baseline): not ours, so it is not turned off.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: false, scope: true)
            let result = add(rig, confirm: { [fake = rig.fake] _ in
                fake.put("sharing", "on")
                return true
            })
            var refused = false
            if case .ctl(5, _)? = errorOf(result) { refused = true }
            expect(refused && !rig.fake.lines("log").contains("sharing-off") && rig.fake.read("sharing") == "on",
                "SSH rollback: a refused sharing on leaves sharing as someone else set it")
        }

        // `sharing on` applied but its answer lost: the rollback turns it off.
        if let rig = Rig(callTimeout: 3) {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: false, scope: true)
            rig.fake.setMode("sharing-hang")
            let result = add(rig)
            rig.fake.setMode(nil)
            expect(errorOf(result) == .timedOut && rig.fake.lines("log").contains("sharing-off") && rig.fake.read("sharing") == "off",
                "SSH rollback: a sharing on whose answer is lost is turned off again")
        }

        // A re-approval that fails after a claim retry moved the forward: the saved forward comes back.
        if let rig = Rig() {
            defer { rig.cleanup() }
            rig.fake.seed(running: true, sharing: true, scope: true)
            if let original = value(add(rig)) {
                rig.fake.put("host", "") // no listener: the saved port is listened on first
                rig.fake.put("grants", "")
                rig.local.failClaims = 1
                rig.local.pendingSas = "0000-1111"
                let result = blocking { [coordinator = rig.coordinator] in
                    try await coordinator.reapprove(id: original.id, confirm: { _ in true }, confirmIdentityChange: { _, _ in true })
                }
                let moved = rig.tunnels.started.contains { $0.id == original.id && $0.remotePort != original.remotePort }
                let kept = moved && rig.tunnels.started.last == original && !rig.tunnels.stopped.contains(original.id)
                    && rig.store.all() == [original] && errorOf(result) != nil
                // A failed add still stops its forward.
                _ = add(rig, confirm: { _ in true }, host: "box2.example")
                expect(kept && rig.tunnels.stopped.count == 1 && rig.tunnels.stopped.last == rig.tunnels.started.last?.id,
                    "SSH rollback: a failed re-approval restarts the saved forward (its remote port); a failed add stops its own")
            } else { expect(false, "SSH rollback: re-approve setup") }
        }
    }

    // MARK: Manual commands

    static func manualCommandChecks(_ expect: Expect) {
        guard let target = try? SSHTarget(user: "me", host: host, port: 2222) else { return }
        let grant = String(repeating: "ab", count: 16)
        let commands: [SSHCtlCommand] = [.grantRevoke(grantId: grant), .status, .sharing(false), .shutdown, .uninstall(purge: true)]
        let shells = commands.compactMap { try? $0.remoteCommand().shell }
        let lines = SSHManualCommands.lines(commands, on: target)
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("syrtis-term-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: tmp) }
        let out = tmp.appendingPathComponent("argv")
        try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        let recorder = tmp.appendingPathComponent("ssh")
        try? "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > \"\(out.path)\"\n"
            .write(to: recorder, atomically: true, encoding: .utf8)
        try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: recorder.path)
        var parsed: [[String]] = []
        for line in lines {
            try? FileManager.default.removeItem(at: out)
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/bin/sh")
            process.arguments = ["-c", line]
            process.environment = ["PATH": "\(tmp.path):/usr/bin:/bin", "HOME": "/nonexistent-home-sentinel"]
            try? process.run()
            process.waitUntilExit()
            let data = (try? Data(contentsOf: out)) ?? Data()
            parsed.append(String(decoding: data, as: UTF8.self).split(separator: "\0", omittingEmptySubsequences: false)
                .dropLast().map(String.init))
        }
        expect(lines.count == shells.count && parsed.count == shells.count
            && zip(parsed, shells).allSatisfy { $0 == ["-p", "2222", "--", "me@\(host)", $1] }
            && parsed.allSatisfy { ($0.last ?? "").hasPrefix("sh -c '") && ($0.last ?? "").contains("$HOME") },
            "SSH manual commands: /bin/sh parses each line into the exact argv the app passes, the last being the sh -c command (\(parsed.count))")
        expect(SSHRemoteCommand.terminalQuoted("a'b $HOME") == "'a'\\''b $HOME'",
            "SSH manual commands: a single quote inside is closed, escaped and reopened")
    }
}
