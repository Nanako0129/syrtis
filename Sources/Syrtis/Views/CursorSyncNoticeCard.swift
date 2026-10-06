import SwiftUI
import TokenBarCore

/// The one-time notice before the first Cursor sync (plan D3). A card on the
/// global Overview, like the other first-run cards: it appears where the
/// person already is, needs no modal and no extra window, and sync waits for
/// the answer. Not counted in the "Set up Syrtis" header — it is a consent, not
/// a preference, and it carries its own answer key. Shown only when the Cursor
/// app has run on this Mac.
struct CursorSyncNoticeCardView: View {
    @AppStorage(CursorSync.enabledKey) private var enabled = true
    @AppStorage(CursorSync.noticeKey) private var acknowledged = false

    var body: some View {
        let _ = (enabled, acknowledged)
        OnboardingCardContainer(visible: CursorSync.noticeVisible()) {
            DashCard(CursorSync.Copy.title.localized) {
                VStack(alignment: .leading, spacing: 8) {
                    Text(CursorSync.Copy.privacy.localized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 10) {
                        Spacer()
                        Button(CursorSync.Copy.turnOff.localized) {
                            CursorSyncController.shared.answerNotice(continuing: false)
                        }
                        .buttonStyle(.plain)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        Button(CursorSync.Copy.continue.localized) {
                            CursorSyncController.shared.answerNotice(continuing: true)
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                    }
                }
            }
            .onboardingCardStyle()
        }
    }
}
