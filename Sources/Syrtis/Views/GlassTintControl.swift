import SwiftUI

/// The glass tint slider (#490), one control for every place it appears:
/// Settings, the one-time guide card and the popover's quick settings. All
/// three write the same `GlassPanelStyle.glassTintKey`.
struct GlassTintSlider: View {
    @AppStorage(GlassPanelStyle.glassTintKey) private var glassTint = 0.0

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "rectangle.on.rectangle")
                .foregroundStyle(.secondary)
            Slider(value: $glassTint, in: 0...1)
                .controlSize(.small)
                .accessibilityLabel("Glass tint".localized)
            Image(systemName: "rectangle.fill.on.rectangle.fill")
                .foregroundStyle(.secondary)
        }
    }

    /// The tint only does anything where the glass panel exists, and
    /// StatusItemController creates it only on macOS 27+. Before that the
    /// popover is an NSPopover with no `.glassEffect` surface to tint.
    static var glassAvailable: Bool {
        glassAvailable(on: ProcessInfo.processInfo.operatingSystemVersion)
    }

    static func glassAvailable(on version: OperatingSystemVersion) -> Bool {
        version.majorVersion >= 27
    }
}

/// Quick settings, opened from the popover header: settings worth changing
/// without the Settings window. Glass tint only for now (maintainer,
/// 2026-10-08); add a row here when another setting earns a place.
struct QuickSettingsCard: View {
    var body: some View {
        DashCard("Quick settings".localized) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Glass tint".localized)
                    .font(.caption)
                GlassTintSlider()
            }
        }
    }
}
