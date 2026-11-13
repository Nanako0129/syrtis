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
    /// Must equal `agent_factory::KEYCHAIN_SERVICE`; SelfTest pins the literal.
    static let keychainService = "Syrtis Factory API Key"
    static let keychainAccount = "factory"
    private static let securityTool = "/usr/bin/security"
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

    /// Saves `key` (replacing any stored one) and reports whether the item is
    /// present afterwards. Success is read back from the Keychain rather than
    /// taken from the exit status, because the item being there is what the
    /// Droid card depends on.
    static func store(_ key: String) async -> Bool {
        let trimmed = key.trimmingCharacters(in: .whitespacesAndNewlines)
        guard isWellFormed(trimmed) else { return false }
        return await Task.detached(priority: .userInitiated) {
            let command = "add-generic-password -U -s \"\(keychainService)\" -a \"\(keychainAccount)\" "
                + "-l \"\(keychainService)\" -w \"\(trimmed)\"\n"
            _ = runSecurity(["-i"], stdin: command)
            return status() == .present
        }.value
    }

    /// Deletes the stored key and reports whether it is gone afterwards.
    static func clear() async -> Bool {
        await Task.detached(priority: .userInitiated) {
            _ = runSecurity(["delete-generic-password", "-s", keychainService])
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
        switch runSecurity(["find-generic-password", "-s", keychainService]) {
        case 0: return .present
        case itemNotFound: return .absent
        default: return .unreadable
        }
    }

    private static func runSecurity(_ arguments: [String], stdin: String? = nil) -> Int32? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: securityTool)
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        let input = Pipe()
        process.standardInput = stdin == nil ? FileHandle.nullDevice : input
        do {
            try process.run()
        } catch {
            return nil
        }
        if let stdin {
            input.fileHandleForWriting.write(Data(stdin.utf8))
            try? input.fileHandleForWriting.close()
        }
        process.waitUntilExit()
        return process.terminationStatus
    }
}
