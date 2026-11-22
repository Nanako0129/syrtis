import Foundation

/// The Factory API key the Droid quota card reads (`agent_factory.rs`).
///
/// The key lives in the login Keychain and is written, read and deleted only
/// through `/usr/bin/security`. An item created by that tool lists it as a
/// trusted application, so the core's read (also `/usr/bin/security`) raises
/// no authorization dialog. A `SecItem` write from this process would make the
/// app the trusted one instead, and the core's read would then prompt.
///
/// The key reaches `security` on stdin through its interactive mode, never on
/// argv, so it does not appear in the process table. It is never written to
/// `UserDefaults`, a log, or a file.
enum FactoryAPIKey {
    /// Must equal `agent_factory::KEYCHAIN_SERVICE` / `KEYCHAIN_ACCOUNT`;
    /// SelfTest pins the literals. Every call names both, so an unrelated item
    /// sharing the service is never read, overwritten or deleted.
    static let keychainService = "com.nyanako.tokenbar.factory-api-key"
    static let keychainAccount = "factory"
    /// The name Keychain Access shows; the Settings copy quotes it.
    static let keychainLabel = "Factory API key for Syrtis"
    private static let securityTool = "/usr/bin/security"
    /// No `security` call here involves a prompt, so one still running after
    /// this long is wedged; it is terminated and the call reports failure.
    private static let securityTimeout: TimeInterval = 10
    /// `security(1)` exit status for "The specified item could not be found".
    private static let itemNotFound: Int32 = 44

    enum Status: Sendable, Equatable {
        case absent
        case present
        /// `security` failed for a reason other than a missing item, for
        /// example a locked Keychain.
        case unreadable
    }

    /// The characters a Factory API key uses. Anything else — a quote, a
    /// backslash, whitespace inside the key — is refused before it reaches
    /// `security -i`, whose command line it would otherwise be parsed into.
    static func isWellFormed(_ key: String) -> Bool {
        let trimmed = key.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.range(of: #"^[A-Za-z0-9_-]{20,256}$"#, options: .regularExpression) != nil
    }

    static func loadStatus() async -> Status {
        await Task.detached(priority: .userInitiated) { status() }.value
    }

    /// Saves `key` (replacing any stored one) and reports success only when
    /// `security -i` exited 0 and the item is present afterwards: the exit
    /// status says this write worked, the read-back says the item the Droid
    /// card depends on is there.
    static func store(_ key: String) async -> Bool {
        let trimmed = key.trimmingCharacters(in: .whitespacesAndNewlines)
        guard isWellFormed(trimmed) else { return false }
        return await Task.detached(priority: .userInitiated) {
            let command = "add-generic-password -U -s \"\(keychainService)\" -a \"\(keychainAccount)\" "
                + "-l \"\(keychainLabel)\" -w \"\(trimmed)\"\n"
            let exitStatus = runSecurity(["-i"], stdin: command)
            return exitStatus == 0 && status() == .present
        }.value
    }

    /// Deletes the stored key and reports whether it is gone afterwards.
    static func clear() async -> Bool {
        await Task.detached(priority: .userInitiated) {
            _ = runSecurity(["delete-generic-password", "-s", keychainService, "-a", keychainAccount])
            return status() == .absent
        }.value
    }

    /// Make the Droid card follow a save or removal now instead of at the next
    /// 60s/5min poll: drop the throttled payload first, then wake the polls,
    /// the same order `GrokBotKeychainConsent.install` uses.
    @MainActor
    static func refreshQuota() async {
        await AgentUsageThrottle.shared.invalidate()
        ClaudeExtraRoots.RegistryChange.signal()
    }

    private static func status() -> Status {
        // Attributes only (no `-w`), so the secret never leaves the Keychain here.
        switch runSecurity(["find-generic-password", "-s", keychainService, "-a", keychainAccount]) {
        case 0: return .present
        case itemNotFound: return .absent
        default: return .unreadable
        }
    }

    /// The exit status, or nil when `security` could not start or did not
    /// finish within `securityTimeout` (it is terminated then).
    private static func runSecurity(_ arguments: [String], stdin: String? = nil) -> Int32? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: securityTool)
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        let input = Pipe()
        process.standardInput = stdin == nil ? FileHandle.nullDevice : input
        let exited = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in exited.signal() }
        do {
            try process.run()
        } catch {
            return nil
        }
        if let stdin {
            let writer = input.fileHandleForWriting
            // If `security` exits before reading, the write must fail with
            // EPIPE rather than raise SIGPIPE, whose default action would
            // terminate the app. The throwing write reports it instead of
            // raising an Objective-C exception.
            _ = fcntl(writer.fileDescriptor, F_SETNOSIGPIPE, 1)
            do {
                try writer.write(contentsOf: Data(stdin.utf8))
            } catch {
                // The exit status (or the timeout) below reports the failure.
            }
            try? writer.close()
        }
        guard exited.wait(timeout: .now() + securityTimeout) == .success else {
            process.terminate()
            return nil
        }
        return process.terminationStatus
    }
}
