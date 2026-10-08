import SwiftUI
import TokenBarCore

/// The setup cards on the global Overview (`OnboardingSetup`). They all show
/// at once, each answers on its own, and each animates away through
/// `OnboardingCardContainer` when answered.
enum OnboardingSetupCopy {
    static let headerTitle = "Set up Syrtis"
    static let headerRemaining = "%lld left · everything here is also in Settings"
    static let skipAll = "Skip setup"

    static let agentsTitle = "Agents on this Mac"
    static let agentsBody = "Syrtis found these agents on this Mac. Choose which ones get a tab, or keep them all."
    static let agentsNone = "No agents found yet. A tab appears once an agent writes its logs or reports a limit."
    static let chooseTabs = "Choose tabs…"
    static let looksGood = "Looks good"

    static let iconTitle = "Menu-bar icon"
    static let iconBody = "Animated icons follow your token rate; gauges drain as a quota window empties."

    static let titleTitle = "Menu-bar text"
    static let titleBody = "What shows next to the icon."

    static let done = "Done"

    static let loginTitle = "Start at login"
    static let loginBody = "Open Syrtis when you log in to your Mac, so your usage is always in the menu bar."
    static let loginOn = "Start at login"
    static let loginOff = "Not now"
    static let loginAlreadyOn = "Syrtis already starts at login."
    static let loginFailed = "macOS did not add Syrtis to your login items. Check System Settings → General → Login Items."

    static let discordTitle = "Discord"
    static let discordBody = "Your Discord profile can show today's usage. It is off unless you turn it on, and Settings shows exactly what would appear."
    static let discordAlreadyOn = "Discord is showing your usage. Settings has what appears and how to stop it."
    static let discordSetUp = "Set up in Settings…"
    static let discordNo = "Not now"

    static var all: [String] {
        [headerTitle, headerRemaining, skipAll, agentsTitle, agentsBody, agentsNone, chooseTabs,
         looksGood, iconTitle, iconBody, titleTitle, titleBody, done, loginTitle, loginBody,
         loginOn, loginOff, loginAlreadyOn, loginFailed, discordTitle, discordBody, discordAlreadyOn,
         discordSetUp, discordNo]
    }
}

/// Every setup card, in order, for the global Overview.
struct OnboardingSetupCards: View {
    var presentClients: [String]
    var modelReport: ModelReport?
    var agentUsage: AgentUsagePayload?

    /// Observed so every card and the header count redraw when any answer,
    /// or a setting a card reflects, changes.
    @AppStorage(OnboardingSetup.completedKey) private var completed = false
    @AppStorage(OnboardingSetup.answeredKey(.agents)) private var agentsAnswered = false
    @AppStorage(OnboardingSetup.answeredKey(.icon)) private var iconAnswered = false
    @AppStorage(OnboardingSetup.answeredKey(.title)) private var titleAnswered = false
    @AppStorage(OnboardingSetup.answeredKey(.login)) private var loginAnswered = false
    @AppStorage(OnboardingSetup.answeredKey(.discord)) private var discordAnswered = false
    @AppStorage(TrayAnimator.styleKey) private var style = "cat"
    @AppStorage(TrayAnimator.animateKey) private var animate = true
    @AppStorage(AnimationPace.storageKey) private var paceRaw = ""
    @AppStorage(AnimationPaceOnboarding.answeredKey) private var paceAnswered = false
    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(UsageAttribution.confirmedKey) private var attributionRaw = ""
    @AppStorage(AttributionOnboardingCard.dismissedKey) private var attributionDismissed = false

    private var userRuntime: Bool { !BuildIdentity.isNonUserRuntime(CommandLine.arguments) }
    private var loginAvailable: Bool { AutostartService.isAvailable }

    private func shows(_ step: OnboardingSetup.Step, _ answered: Bool) -> Bool {
        userRuntime && !completed && !answered
    }

    /// Computed once per body and passed to the header: the attribution half
    /// walks the whole model report, and used to be evaluated separately for
    /// the header's visibility and for its text.
    private func remaining(attributionShows: Bool) -> Int {
        OnboardingSetup.remaining(
            loginAvailable: loginAvailable,
            paceCardShows: AnimationPaceOnboarding.isVisible(
                style: style, animate: animate, answered: paceAnswered,
                isNonUserRuntime: !userRuntime),
            attributionCardShows: attributionShows)
    }

    var body: some View {
        let _ = (attributionRaw, attributionDismissed, paceRaw)
        let left = remaining(attributionShows: AttributionOnboardingCard.shows(
            modelReport: modelReport, agentUsage: agentUsage))
        // No stack spacing: each container carries its own gap (see
        // `OnboardingCardContainer.gap`).
        VStack(spacing: 0) {
            OnboardingCardContainer(visible: userRuntime && left > 0) { header(left) }
            OnboardingCardContainer(visible: shows(.agents, agentsAnswered)) { agentsCard }
            OnboardingCardContainer(visible: shows(.icon, iconAnswered)) { iconCard }
            OnboardingCardContainer(visible: shows(.title, titleAnswered)) { titleCard }
            AnimationPaceOnboardingCardView()
            AttributionOnboardingCardView(modelReport: modelReport, agentUsage: agentUsage)
            CursorSyncNoticeCardView()
            GlassTintGuideCardView()
            OnboardingCardContainer(visible: loginAvailable && shows(.login, loginAnswered)) {
                LoginCard()
            }
            OnboardingCardContainer(visible: shows(.discord, discordAnswered)) { discordCard }
        }
    }

    // MARK: - Header

    private func header(_ remaining: Int) -> some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text(OnboardingSetupCopy.headerTitle.localized).font(.headline)
                Text(OnboardingSetupCopy.headerRemaining.localized(Int64(remaining)))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button(OnboardingSetupCopy.skipAll.localized) { OnboardingSetup.skipAll() }
                .buttonStyle(.plain)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .onboardingCardStyle()
    }

    // MARK: - Agents

    private var agentsCard: some View {
        DashCard(OnboardingSetupCopy.agentsTitle) {
            VStack(alignment: .leading, spacing: 8) {
                Text(OnboardingSetupCopy.agentsBody.localized)
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if presentClients.isEmpty {
                    Text(OnboardingSetupCopy.agentsNone.localized)
                        .font(.caption2).foregroundStyle(.tertiary)
                } else {
                    Text(presentClients.map { ClientRegistry.style($0).displayName }
                        .joined(separator: " · "))
                        .font(.caption)
                        .fixedSize(horizontal: false, vertical: true)
                }
                answerRow(
                    secondary: (OnboardingSetupCopy.chooseTabs, {
                        SettingsWindowController.shared.showFromPopover(scrollingTo: .dashboard)
                        OnboardingSetup.answer(.agents)
                    }),
                    primary: (OnboardingSetupCopy.looksGood, { OnboardingSetup.answer(.agents) }))
            }
        }
        .onboardingCardStyle()
    }

    // MARK: - Icon

    private var iconCard: some View {
        DashCard(OnboardingSetupCopy.iconTitle) {
            VStack(alignment: .leading, spacing: 8) {
                Text(OnboardingSetupCopy.iconBody.localized)
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                // Picking applies at once, so the live menu-bar icon can be
                // tried; "Done" answers the card.
                choiceGrid(options: TrayAnimator.iconStyleOptions, selected: style) { style = $0 }
                doneRow { OnboardingSetup.answer(.icon) }
            }
        }
        .onboardingCardStyle()
    }

    // MARK: - Title

    private var titleCard: some View {
        DashCard(OnboardingSetupCopy.titleTitle) {
            VStack(alignment: .leading, spacing: 8) {
                Text(OnboardingSetupCopy.titleBody.localized)
                    .font(.caption).foregroundStyle(.secondary)
                choiceGrid(
                    options: TrayMode.allCases.map { ($0.rawValue, $0.label) },
                    selected: trayModeRaw
                ) { trayModeRaw = $0 }
                doneRow { OnboardingSetup.answer(.title) }
            }
        }
        .onboardingCardStyle()
    }

    // MARK: - Discord

    private var discordCard: some View {
        DashCard(OnboardingSetupCopy.discordTitle) {
            VStack(alignment: .leading, spacing: 8) {
                Text((DiscordPresence.enabled()
                    ? OnboardingSetupCopy.discordAlreadyOn : OnboardingSetupCopy.discordBody).localized)
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                // Two buttons of equal weight, neither filled nor the default:
                // a prominent "set up" next to a plain "no" is a thumb on the
                // scale, and this card only points at the Settings disclosure.
                HStack(spacing: 10) {
                    Spacer()
                    if DiscordPresence.enabled() {
                        Button(OnboardingSetupCopy.done.localized) {
                            OnboardingSetup.perform(.notNow) {}
                        }
                        .buttonStyle(.bordered).controlSize(.small)
                    } else {
                        Button(OnboardingSetupCopy.discordNo.localized) {
                            OnboardingSetup.perform(.notNow) {}
                        }
                        .buttonStyle(.bordered).controlSize(.small)
                        Button(OnboardingSetupCopy.discordSetUp.localized) {
                            OnboardingSetup.perform(.setUp) {
                                SettingsWindowController.shared.showFromPopover(scrollingTo: .discord)
                            }
                        }
                        .buttonStyle(.bordered).controlSize(.small)
                    }
                }
            }
        }
        .onboardingCardStyle()
    }

    // MARK: - Shared controls

    private func choiceGrid(
        options: [(value: String, label: String)], selected: String,
        pick: @escaping (String) -> Void
    ) -> some View {
        LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 6) {
            ForEach(options, id: \.value) { option in
                Button { pick(option.value) } label: {
                    Text(option.label.localized)
                        .font(.caption)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 5)
                        .background(
                            Color.accentColor.opacity(option.value == selected ? 0.28 : 0.08),
                            in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                        .overlay(
                            RoundedRectangle(cornerRadius: 7, style: .continuous)
                                .strokeBorder(
                                    Color.accentColor.opacity(option.value == selected ? 0.7 : 0.2),
                                    lineWidth: 1))
                }
                .buttonStyle(.plain)
            }
        }
    }

    private func doneRow(_ done: @escaping () -> Void) -> some View {
        HStack {
            Spacer()
            Button(OnboardingSetupCopy.done.localized, action: done)
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
        }
    }

    private func answerRow(
        secondary: (String, () -> Void), primary: (String, () -> Void)
    ) -> some View {
        HStack(spacing: 10) {
            Spacer()
            Button(secondary.0.localized, action: secondary.1)
                .buttonStyle(.plain)
                .font(.caption)
                .foregroundStyle(.secondary)
            Button(primary.0.localized, action: primary.1)
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
        }
    }
}

/// Reads the login item asynchronously, so the card can say it is already on
/// for an existing user instead of offering to turn it on.
private struct LoginCard: View {
    @State private var enabled: Bool?
    @State private var failed = false

    var body: some View {
        DashCard(OnboardingSetupCopy.loginTitle) {
            VStack(alignment: .leading, spacing: 8) {
                Text((enabled == true ? OnboardingSetupCopy.loginAlreadyOn : OnboardingSetupCopy.loginBody)
                    .localized)
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if failed {
                    Text(OnboardingSetupCopy.loginFailed.localized)
                        .font(.caption2).foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
                HStack(spacing: 10) {
                    Spacer()
                    if enabled == true {
                        Button(OnboardingSetupCopy.done.localized) { OnboardingSetup.answer(.login) }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.small)
                    } else {
                        Button(OnboardingSetupCopy.loginOff.localized) { OnboardingSetup.answer(.login) }
                            .buttonStyle(.plain)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Button(OnboardingSetupCopy.loginOn.localized) {
                            // Answered only when it took: a failed register
                            // (for example Syrtis switched off under Login
                            // Items) keeps the card, with the reason.
                            if AutostartService.setEnabled(true) {
                                OnboardingSetup.answer(.login)
                            } else {
                                failed = true
                            }
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .disabled(!AutostartService.isAvailable)
                    }
                }
            }
        }
        .onboardingCardStyle()
        .task { enabled = await AutostartService.readEnabled() }
    }
}
