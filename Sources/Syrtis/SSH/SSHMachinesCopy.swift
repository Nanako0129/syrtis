// The SSH machines section's copy. English is the key; zh-Hant and zh-Hans are in
// Localizable.strings, and the selftest checks every key in `all` has both.
enum SSHMachinesCopy {
    static let section = "SSH machines"
    static let summary = "Read usage from Linux machines you can reach over SSH."
    static let details = "Syrtis installs its usage agent on the machine over your ssh, keeps an ssh tunnel to it, and pairs through the tunnel. It uses your ssh config and ssh-agent, and never accepts an unknown host key. The machine's usage then appears in the device menu like any linked device."
    static let noAgent = "This version doesn't include the remote agent."
    static let noMachines = "No SSH machines."
    static let addHeader = "Add a machine"
    static let user = "User (optional)"
    static let host = "Host or ssh alias"
    static let sshPort = "SSH port (optional)"
    static let label = "Label (optional)"
    static let addNew = "New"
    static let addDeploy = "Add and deploy"
    static let working = "Working on the machine…"
    static let consentTitle = "Approve the scan scope on %@?"
    static let reapproveTitle = "Re-approve %@?"
    static let consentSummary = "The agent will scan the usage records of every client it supports on that machine. Nothing is shared yet. Cancel undoes what was done on the machine so far, as listed below."
    static let baselineHeader = "The machine now"
    static let sharingOn = "Sharing is on."
    static let sharingOff = "Sharing is off."
    static let grantCount = "Devices with access: %lld"
    static let listening = "Listening on %@"
    static let notListening = "Not listening."
    static let scopeAlready = "The scan scope is already approved there."
    static let scopeNotYet = "The scan scope isn't approved there yet."
    static let endsOthers = "Approving a new scan scope ends access for the other devices linked to that machine (%lld). They have to be approved again."
    static let turnsOn = "Sharing will be turned on there."
    static let scopeHeader = "What it will read"
    static let approvePair = "Approve and pair"
    static let identityTitle = "The machine's identity changed"
    static let identityMessage = "Known: %@\nNow: %@\nAccept it only if you reinstalled or reset the agent on that machine."
    static let identityAccept = "Accept new identity"
    static let forwarding = "Connected"
    static let connecting = "Connecting…"
    static let retrying = "Disconnected. Retrying in %lld s."
    static let stopped = "Stopped"
    static let portInUse = "Port in use"
    static let portBusy = "Local port %lld is in use. Remove the machine and add it again."
    static let needsReapproval = "Needs re-approval."
    static let reapprove = "Re-approve"
    static let hostKey = "The host key isn't trusted yet. Run “%@” in a terminal once to confirm it, then try again."
    static let removeTitle = "Remove %@?"
    static let removeMessage = "Syrtis revokes its own access on the machine and stops reading it. Other devices' access there is left alone."
    static let purge = "Also uninstall the agent and delete its data there (uninstall --purge), unless other devices still use it"
    static let identityKept = "Re-approved. The machine's identity changed and you accepted the new one."
    static let errInvalid = "Check the %@ field."
    static let errLaunch = "Couldn't start /usr/bin/ssh."
    static let errUnreachable = "Couldn't reach the machine: %@"
    static let errUnreachableBare = "Couldn't reach the machine."
    static let errTimeout = "The machine didn't answer in time."
    static let errOutput = "The machine sent more output than expected."
    static let errMalformed = "The machine answered with unexpected output. A shell startup file there may print text."
    static let errPlatform = "Only Linux x86_64 machines are supported."
    static let errIntegrity = "The agent didn't match its pinned checksum. Nothing was installed."
    static let errAgentMissing = "The agent isn't installed on the machine."
    static let errCommand = "The agent command failed (%lld). The machine's folder permissions or state directory may have failed a check. %@"
    static let errUnsafeDirs = "These folders on the machine can be written by group or others, so the agent can't be installed: %@. Nothing was uploaded or changed. Run “%@” in a terminal, then try again."
    static let errBusy = "Another machine operation is running."
    static let errNoPort = "No free local port in 47100-47999."
    static let errDeclined = "Not confirmed. What this step did on the machine was undone."
    static let errNotLoopback = "The agent there already listens on another address, so it was left alone."
    static let errTunnel = "The SSH tunnel didn't come up: %@"
    static let errTunnelBare = "The SSH tunnel didn't come up."
    static let errPairing = "Pairing failed (%@)."
    static let errIdentity = "The machine's identity changed and wasn't accepted."
    static let errCancelled = "Cancelled."
    static let errTruncated = "The scan scope can't be shown in full, so it wasn't approved."
    static let errOther = "Something went wrong. Try again."

    static let agentInstalled = "The agent was installed just now. Cancel uninstalls it again."
    static let agentReplaced = "An agent was already installed there, so it was stopped, replaced and restarted. Cancel restarts it; the old build is not restored."
    static let agentStarted = "The agent there wasn't running, so it was started. Cancel stops it again."
    static let agentUnchanged = "The agent there is already running and was left as it is."
    static let scopeTruncated = "This list was cut off or changed for display (over 64 lines or 512 characters, or it had control characters). The approval covers all of it, so it can't be approved until it can be shown in full."
    static let manualNotice = "Syrtis couldn't finish this on the machine. To finish it yourself, run:\n%@"
    static let purgeSkipped = "Other devices (%lld) still use the agent there, so it was left installed."

    static let all = [
        section, summary, details, noAgent, noMachines, addHeader,
        user, host, sshPort, label, addNew, addDeploy, working,
        consentTitle, reapproveTitle, consentSummary, baselineHeader, sharingOn, sharingOff,
        grantCount, listening, notListening, scopeAlready, scopeNotYet, endsOthers,
        turnsOn, scopeHeader, approvePair, identityTitle, identityMessage, identityAccept,
        connecting, retrying, stopped, portBusy,
        needsReapproval, reapprove, hostKey, removeTitle, removeMessage, purge,
        identityKept, errInvalid, errLaunch, errUnreachable, errUnreachableBare, errTimeout, errOutput, errMalformed,
        errPlatform, errIntegrity, errAgentMissing, errCommand, errBusy, errNoPort,
        errDeclined, errNotLoopback, errTunnel, errTunnelBare, errPairing, errIdentity,
        errCancelled, errOther, errUnsafeDirs, portInUse,
        errTruncated, agentInstalled, agentReplaced, agentStarted, agentUnchanged, scopeTruncated,
        purgeSkipped, manualNotice,
    ]
}
