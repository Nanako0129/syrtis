import SwiftUI

/// One-time guide to Settings › Glass tint (#490), styled like the other
/// first-run cards on the Overview. Over a window of the opposite brightness
/// the glass panel can wash its text out; most people would never find the
/// slider in Settings, so the card puts the same slider where they already are.
/// Done hides it for good; the slider stays in Quick settings and Settings.
struct GlassTintGuideCardView: View {
    static let dismissedKey = "tokenbar.glass.tintGuideDismissed"

    @AppStorage(Self.dismissedKey) private var dismissed = false

    /// Shown only where the glass panel exists (macOS 26+) and in a real user
    /// session, until Done.
    static func visible(dismissed: Bool, glassAvailable: Bool, userRuntime: Bool) -> Bool {
        glassAvailable && userRuntime && !dismissed
    }

    var body: some View {
        OnboardingCardContainer(visible: Self.visible(
            dismissed: dismissed, glassAvailable: GlassTintSlider.glassAvailable,
            userRuntime: CursorSync.isUserRuntime(CommandLine.arguments)))
        {
            DashCard("Glass tint".localized) {
                VStack(alignment: .leading, spacing: 8) {
                    Text(Self.copyBody.localized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    GlassTintSlider()
                    HStack {
                        Spacer()
                        Button("Done".localized) { dismissed = true }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.small)
                    }
                }
            }
            .onboardingCardStyle()
        }
    }

    static let copyBody = "Text hard to read over the windows behind this one? Make the glass less see-through here. You can change it later in Quick settings at the top right."
}
