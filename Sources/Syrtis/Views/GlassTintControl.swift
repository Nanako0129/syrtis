import SwiftUI

/// The live glass tint every slider and the panel surface share. Dragging a slider used to write
/// `GlassPanelStyle.glassTintKey` to UserDefaults on every step (`@AppStorage`); each write wakes the app's three
/// `didChangeNotification` observers on the main thread, the same cost that made the 3D chart's drag stutter
/// (#498). The slider now moves this in-memory value, and the default is written once, `saveDelay` after the
/// last change, with the value captured when scheduled so it lands even if the slider's view goes away.
@MainActor
final class GlassTint: ObservableObject {
    static let shared = GlassTint()
    static let saveDelay: TimeInterval = 0.5

    @Published var value: Double {
        didSet { scheduleSave() }
    }
    private var pendingSave: DispatchWorkItem?
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        value = defaults.double(forKey: GlassPanelStyle.glassTintKey)
    }

    private func scheduleSave() {
        pendingSave?.cancel()
        let raw = value
        let defaults = self.defaults
        let save = DispatchWorkItem { defaults.set(raw, forKey: GlassPanelStyle.glassTintKey) }
        pendingSave = save
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.saveDelay, execute: save)
    }
}

/// The glass tint slider (#490), one control for every place it appears:
/// Settings, the one-time guide card and the popover's quick settings. All
/// three move the shared `GlassTint`.
struct GlassTintSlider: View {
    @ObservedObject private var glassTint = GlassTint.shared

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "rectangle.on.rectangle")
                .foregroundStyle(.secondary)
            Slider(value: $glassTint.value, in: 0...1)
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
