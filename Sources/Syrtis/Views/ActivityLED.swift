import AppKit
import SwiftUI

/// The live-rate badge's green activity light, flickered by Core Animation.
///
/// It used to be a `TimelineView(.periodic(by: 0.09))` redrawing a SwiftUI
/// circle. While tokens flowed, that re-rendered the whole popover hosting
/// view on every display frame — measured at ~120 renders a second and over
/// half a core on the main thread with the popover open. A keyframe animation
/// on a layer runs in the render server, so the popover's view graph is no
/// longer touched per frame.
struct ActivityLED: NSViewRepresentable {
    /// Percent chance (0–100) that a slot is a brief off-blink.
    let offChance: Int

    /// One flicker cycle: 67 slots of 90 ms (~6 s), long enough that the
    /// repetition does not read as a loop.
    static let slotSeconds = 0.09
    static let slotCount = 67

    /// Lit or not, per slot: the same multiplicative hash the TimelineView
    /// applied to the wall-clock slot, applied to the slot index instead.
    static func pattern(offChance: Int) -> [Bool] {
        (0..<UInt64(slotCount)).map { slot in
            let hash = (slot &* 0x9E37_79B9_7F4A_7C15) >> 33
            return Int(hash % 100) >= offChance
        }
    }

    func makeNSView(context: Context) -> LEDView {
        let view = LEDView()
        view.setOffChance(offChance)
        return view
    }

    func updateNSView(_ view: LEDView, context: Context) {
        view.setOffChance(offChance)
    }

    final class LEDView: NSView {
        private let dot = CALayer()
        private var offChance: Int?

        override init(frame: NSRect) {
            super.init(frame: frame)
            wantsLayer = true
            dot.frame = CGRect(x: 0, y: 0, width: 6, height: 6)
            dot.cornerRadius = 3
            dot.shadowOffset = .zero
            dot.shadowRadius = 2
            dot.shadowOpacity = 0.8
            layer?.addSublayer(dot)
            refreshColors()
        }

        required init?(coder: NSCoder) {
            fatalError("init(coder:) has not been implemented")
        }

        // CGColors are resolved once, so re-resolve the dynamic system green
        // when the panel switches between light and dark.
        override func viewDidChangeEffectiveAppearance() {
            super.viewDidChangeEffectiveAppearance()
            refreshColors()
        }

        private func refreshColors() {
            effectiveAppearance.performAsCurrentDrawingAppearance {
                dot.backgroundColor = NSColor.systemGreen.cgColor
                dot.shadowColor = NSColor.systemGreen.cgColor
            }
        }

        /// Rebuilds the flicker only when the off-chance actually changes —
        /// the rate is re-polled every 10 s and usually lands in the same band.
        func setOffChance(_ value: Int) {
            guard value != offChance else { return }
            offChance = value
            let lit = ActivityLED.pattern(offChance: value)
            // Discrete keyframes take one more key time than values.
            let keyTimes = (0...lit.count).map { NSNumber(value: Double($0) / Double(lit.count)) }
            func steps(_ keyPath: String, lit litValue: Double, off offValue: Double) -> CAKeyframeAnimation {
                let animation = CAKeyframeAnimation(keyPath: keyPath)
                animation.calculationMode = .discrete
                animation.values = lit.map { $0 ? litValue : offValue }
                animation.keyTimes = keyTimes
                return animation
            }
            let flicker = CAAnimationGroup()
            flicker.animations = [
                steps("opacity", lit: 1, off: 0.25),
                steps("shadowOpacity", lit: 0.8, off: 0),
            ]
            flicker.duration = ActivityLED.slotSeconds * Double(lit.count)
            flicker.repeatCount = .infinity
            dot.add(flicker, forKey: "flicker")
        }
    }
}
