import AppKit
import SceneKit
import simd
import SwiftUI
import TokenBarCore

// SceneKit port of ContributionGraph3D.tsx, grown from the Phase 2 spike.
// 53x7 grid of SCNBox tiles, per-face top/side materials, intensity color
// ramp, orthographic camera with an OrbitControls-parity custom rig (the
// built-in camera controller's zoom is a dolly — a no-op under orthographic
// projection). Renders on demand; the camera persists across opens.

private let CELL: CGFloat = 1.0
private let GAP: CGFloat = 0.15
private let STEP: CGFloat = CELL + GAP
private let BASE_HEIGHT: CGFloat = 0.05
private let MAX_HEIGHT: CGFloat = 4.0

private let activeLight = GraphRGB(hex: 0xBFDBFE)
private let activeDark = GraphRGB(hex: 0x1E3A8A)

/// Inactive "floor" tiles follow the system appearance — the web version
/// hardcodes white, which reads harsh against the dark popover glass.
private func inactiveColors(dark: Bool) -> (top: GraphRGB, side: GraphRGB) {
    dark
        ? (GraphRGB(hex: 0x3A4150), GraphRGB(hex: 0x2B313D))
        : (GraphRGB(hex: 0xFFFFFF), GraphRGB(hex: 0xEAEDF2))
}

struct GraphRGB {
    var r: CGFloat
    var g: CGFloat
    var b: CGFloat

    init(hex: UInt32) {
        r = CGFloat((hex >> 16) & 0xFF) / 255
        g = CGFloat((hex >> 8) & 0xFF) / 255
        b = CGFloat(hex & 0xFF) / 255
    }

    init(r: CGFloat, g: CGFloat, b: CGFloat) {
        self.r = r
        self.g = g
        self.b = b
    }

    func lerp(_ other: GraphRGB, _ t: CGFloat) -> GraphRGB {
        GraphRGB(r: r + (other.r - r) * t, g: g + (other.g - g) * t, b: b + (other.b - b) * t)
    }

    func scaled(_ f: CGFloat) -> GraphRGB { GraphRGB(r: r * f, g: g * f, b: b * f) }
    var ns: NSColor { NSColor(srgbRed: r, green: g, blue: b, alpha: 1) }
}

private func pbrMaterial(_ color: GraphRGB, roughness: CGFloat) -> SCNMaterial {
    let m = SCNMaterial()
    m.lightingModel = .physicallyBased
    m.diffuse.contents = color.ns
    m.roughness.contents = NSNumber(value: Double(roughness))
    m.metalness.contents = NSNumber(value: 0.04)
    return m
}

// MARK: - Orbit camera rig

/// Custom OrbitControls-parity rig driving azimuth/elevation/orthographicScale
/// directly. State persists to UserDefaults (tokenbar.orbit.v1 in spirit).
@MainActor
final class OrbitRig {
    static let storageKey = "tokenbar.orbit.v1"

    let cameraNode = SCNNode()
    let camera = SCNCamera()
    var target = simd_double3(0, 0, 0)
    var azimuth: Double = .pi / 4
    var elevation: Double = atan2(0.45, sqrt(2.0) * 0.7) // ~24.6° (tsx start pos)
    private let distance: Double = 150 // irrelevant to ortho size; clears clipping
    var scale: Double = 26 // orthographicScale = half view height, world units
    private let minScale: Double = 0.6 // ≈ tsx maxZoom 80 at a ~300px view
    private let maxScale: Double = 160 // ≈ tsx minZoom 1
    private let maxElevation = 89.0 * .pi / 180

    init() {
        camera.usesOrthographicProjection = true
        camera.zNear = -1000
        camera.zFar = 1000
        cameraNode.camera = camera
        restore()
        apply()
    }

    func apply() {
        elevation = min(max(elevation, -maxElevation), maxElevation)
        scale = min(max(scale, minScale), maxScale)
        let cosE = cos(elevation)
        let pos = target + distance * simd_double3(cosE * sin(azimuth), sin(elevation), cosE * cos(azimuth))
        cameraNode.position = SCNVector3(pos.x, pos.y, pos.z)
        cameraNode.look(
            at: SCNVector3(target.x, target.y, target.z),
            up: SCNVector3(0, 1, 0), localFront: SCNVector3(0, 0, -1))
        camera.orthographicScale = scale
    }

    func orbit(dx: CGFloat, dy: CGFloat) {
        azimuth -= Double(dx) * 0.01 * 0.7 // OrbitControls rotateSpeed 0.7
        elevation += Double(dy) * 0.01 * 0.7
        apply()
        persist()
    }

    func pan(dx: CGFloat, dy: CGFloat, viewHeightPx: CGFloat) {
        let worldPerPixel = (2 * scale) / Double(max(viewHeightPx, 1))
        let t = cameraNode.simdWorldTransform
        let right = simd_double3(Double(t.columns.0.x), Double(t.columns.0.y), Double(t.columns.0.z))
        let up = simd_double3(Double(t.columns.1.x), Double(t.columns.1.y), Double(t.columns.1.z))
        target -= right * Double(dx) * worldPerPixel
        target -= up * Double(dy) * worldPerPixel
        apply()
        persist()
    }

    func zoom(deltaY: CGFloat) {
        scale *= exp(Double(deltaY) * 0.02)
        apply()
        persist()
    }

    // MARK: Persistence

    func persist() {
        let values = [azimuth, elevation, scale, target.x, target.y, target.z]
        let raw = values.map { String($0) }.joined(separator: ",")
        UserDefaults.standard.set(raw, forKey: Self.storageKey)
    }

    /// True when a saved camera was restored (skip the initial auto-fit).
    @discardableResult
    func restore() -> Bool {
        guard let raw = UserDefaults.standard.string(forKey: Self.storageKey) else { return false }
        let parts = raw.split(separator: ",").compactMap { Double($0) }
        guard parts.count == 6 else { return false }
        azimuth = parts[0]
        elevation = parts[1]
        scale = parts[2]
        target = simd_double3(parts[3], parts[4], parts[5])
        return true
    }

    static var hasSavedCamera: Bool {
        UserDefaults.standard.string(forKey: storageKey) != nil
    }

    static func clearSavedCamera() {
        UserDefaults.standard.removeObject(forKey: storageKey)
    }

    /// Frame an AABB (already in world space, given as its 8 corners) like the
    /// tsx fitView(): project into camera space, size the ortho scale, and
    /// re-center the target on the box.
    func fit(corners: [simd_double3], viewSize: CGSize) {
        guard !corners.isEmpty else { return }
        target = .zero
        apply()
        let inv = cameraNode.simdWorldTransform.inverse
        var minX = Double.infinity, maxX = -Double.infinity
        var minY = Double.infinity, maxY = -Double.infinity
        for c in corners {
            let v = inv * simd_float4(Float(c.x), Float(c.y), Float(c.z), 1)
            minX = min(minX, Double(v.x))
            maxX = max(maxX, Double(v.x))
            minY = min(minY, Double(v.y))
            maxY = max(maxY, Double(v.y))
        }
        let aspect = Double(viewSize.width / max(viewSize.height, 1))
        let padding = 0.85
        scale = max((maxY - minY) / 2, (maxX - minX) / 2 / aspect) / padding
        // Re-center on the box in camera space.
        let centerX = (minX + maxX) / 2
        let centerY = (minY + maxY) / 2
        let t = cameraNode.simdWorldTransform
        let right = simd_double3(Double(t.columns.0.x), Double(t.columns.0.y), Double(t.columns.0.z))
        let up = simd_double3(Double(t.columns.1.x), Double(t.columns.1.y), Double(t.columns.1.z))
        target += right * centerX + up * centerY
        apply()
        persist()
    }
}

// MARK: - SCNView subclass: input + hover

@MainActor
final class ContributionGraphView: SCNView {
    var rig: OrbitRig!
    var cellByNodeName: [String: GridCell] = [:]
    /// World-space AABB corners of the populated (active) cells, for fit.
    var fitCorners: [simd_double3] = []
    /// Auto-fit on the first layout pass with a real size — fitting before
    /// layout sees a zero-height view and blows the ortho scale to its clamp.
    var needsInitialFit = false
    private var tooltip: NSTextField!
    private var hoveredNode: SCNNode?
    private var scrollZoom: Bool?
    private var scrollPhase: NSEvent.Phase = []

    override func layout() {
        super.layout()
        if needsInitialFit, bounds.width > 0, bounds.height > 0 {
            needsInitialFit = false
            fitToContent()
        }
    }

    func setupTooltip() {
        let t = NSTextField(wrappingLabelWithString: "")
        t.isEditable = false
        t.isSelectable = false
        t.drawsBackground = false
        t.wantsLayer = true
        t.layer?.backgroundColor = NSColor(calibratedWhite: 0.12, alpha: 0.92).cgColor
        t.layer?.cornerRadius = 6
        t.textColor = .white
        t.font = .monospacedSystemFont(ofSize: 10, weight: .medium)
        t.isHidden = true
        addSubview(t)
        tooltip = t
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(
            NSTrackingArea(
                rect: bounds,
                options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                owner: self, userInfo: nil))
    }

    // Camera input: drag = orbit, right/option-drag = pan, command-scroll/pinch = zoom.
    override func mouseDragged(with event: NSEvent) {
        if event.modifierFlags.contains(.option) {
            rig.pan(dx: event.deltaX, dy: event.deltaY, viewHeightPx: bounds.height)
        } else {
            rig.orbit(dx: event.deltaX, dy: event.deltaY)
        }
    }

    override func rightMouseDragged(with event: NSEvent) {
        rig.pan(dx: event.deltaX, dy: event.deltaY, viewHeightPx: bounds.height)
    }

    override func scrollWheel(with event: NSEvent) {
        var zoom = event.modifierFlags.contains(.command)
        if !event.phase.isEmpty || !event.momentumPhase.isEmpty {
            let starts = event.phase.contains(.mayBegin) || event.phase.contains(.began)
            // mayBegin -> began is one gesture; direct ended can precede momentum.
            if scrollZoom == nil || (starts && !scrollPhase.contains(.mayBegin)) {
                scrollZoom = zoom
            }
            zoom = scrollZoom ?? zoom
            scrollPhase = event.phase
        }
        defer {
            // Deliver terminal events to the same responder before forgetting the route.
            if event.phase.contains(.cancelled) || event.momentumPhase.contains(.ended)
                || event.momentumPhase.contains(.cancelled)
            {
                scrollZoom = nil
                scrollPhase = []
            }
        }
        guard zoom else {
            clearHover()
            nextResponder?.scrollWheel(with: event)
            return
        }
        rig.zoom(deltaY: event.scrollingDeltaY)
    }

    override func magnify(with event: NSEvent) {
        rig.zoom(deltaY: -event.magnification * 60)
    }

    func fitToContent() {
        rig.fit(corners: fitCorners, viewSize: bounds.size)
    }

    // Hover: hit-test the tile under the pointer; tooltip only on active days.
    override func mouseMoved(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        handleHover(at: p)
    }

    override func mouseExited(with event: NSEvent) { clearHover() }

    func handleHover(at p: CGPoint) {
        let hits = hitTest(p, options: [
            .searchMode: NSNumber(value: SCNHitTestSearchMode.closest.rawValue),
            .ignoreHiddenNodes: true,
        ])
        guard let hit = hits.first, let name = hit.node.name,
              let cell = cellByNodeName[name], cell.active
        else {
            clearHover()
            return
        }
        if hoveredNode !== hit.node {
            clearHover()
            hoveredNode = hit.node
            for m in hit.node.geometry?.materials ?? [] {
                m.emission.contents = NSColor(srgbRed: 0.15, green: 0.3, blue: 0.9, alpha: 1)
                m.emission.intensity = 0.35
            }
        }
        let tokenLine = "%@ tokens".localized(Format.compactTokens(cell.tokens))
        tooltip.stringValue =
            "\(Format.monthDay(cell.date))\n\(tokenLine)\n\(Format.money(tokens: cell.tokens, cost: cell.cost))"
        tooltip.sizeToFit()
        tooltip.frame = tooltip.frame.insetBy(dx: -7, dy: -5)
        var origin = CGPoint(x: p.x + 12, y: p.y - tooltip.frame.height - 12)
        origin.x = min(origin.x, bounds.width - tooltip.frame.width - 4)
        origin.y = max(origin.y, 4)
        tooltip.setFrameOrigin(origin)
        tooltip.isHidden = false
    }

    private func clearHover() {
        if let n = hoveredNode {
            for m in n.geometry?.materials ?? [] {
                m.emission.contents = NSColor.black
                m.emission.intensity = 0
            }
        }
        hoveredNode = nil
        tooltip?.isHidden = true
    }
}

// MARK: - Scene construction

@MainActor
private func buildGridNode(
    grid: TokenBarCore.GridLayout, dark: Bool
) -> (node: SCNNode, mapping: [String: GridCell], fitCorners: [simd_double3]) {
    let (inactiveTop, inactiveSide) = inactiveColors(dark: dark)
    let totalWidth = CGFloat(grid.cols) * STEP
    let totalDepth = CGFloat(grid.rows) * STEP
    let offsetX = -totalWidth / 2
    let offsetZ = -totalDepth / 2
    let maxTokens = CGFloat(max(grid.maxTokens, 1))

    let gridNode = SCNNode()
    gridNode.name = "grid"
    var mapping: [String: GridCell] = [:]
    var activeCorners: [simd_double3] = []

    for cell in grid.cells where cell.inYear {
        let x = offsetX + CGFloat(cell.col) * STEP + STEP / 2
        let z = offsetZ + CGFloat(cell.row) * STEP + STEP / 2
        var height = BASE_HEIGHT
        var top = inactiveTop
        var side = inactiveSide
        if cell.active {
            let frac = CGFloat(cell.tokens) / maxTokens
            height = BASE_HEIGHT + pow(frac, 0.6) * MAX_HEIGHT
            let t = min(1, max(0, pow(frac, 0.5)))
            top = activeLight.lerp(activeDark, t)
            side = top.scaled(0.78)
            let half = Double(CELL / 2)
            for dx in [-half, half] {
                for dz in [-half, half] {
                    for sy in [0.0, Double(height)] {
                        activeCorners.append(simd_double3(Double(x) + dx, sy, Double(z) + dz))
                    }
                }
            }
        }
        let box = SCNBox(width: CELL, height: height, length: CELL, chamferRadius: 0)
        let topMat = pbrMaterial(top, roughness: 0.5)
        let sideMat = pbrMaterial(side, roughness: 0.6)
        // SCNBox material order: front(+Z) right(+X) back(-Z) left(-X) top(+Y) bottom(-Y).
        box.materials = [sideMat, sideMat, sideMat, sideMat, topMat, sideMat]
        let node = SCNNode(geometry: box)
        node.position = SCNVector3(x, height / 2, z)
        let name = "c\(cell.col)_\(cell.row)"
        node.name = name
        mapping[name] = cell
        gridNode.addChildNode(node)
    }

    // Fall back to the whole grid's AABB when nothing is active yet.
    if activeCorners.isEmpty {
        let halfX = Double(totalWidth / 2)
        let halfZ = Double(totalDepth / 2)
        for sx in [-halfX, halfX] {
            for sz in [-halfZ, halfZ] {
                for sy in [0.0, Double(MAX_HEIGHT)] {
                    activeCorners.append(simd_double3(sx, sy, sz))
                }
            }
        }
    }
    return (gridNode, mapping, activeCorners)
}

// MARK: - SwiftUI wrapper

/// The 3D contribution graph with Fit/Reset controls, equal in footprint to
/// the 2D chart block it toggles with.
struct ContributionGraph3D: View {
    let grid: TokenBarCore.GridLayout

    @State private var holder = GraphHolder()
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        ContributionGraphRepresentable(
            grid: grid, dark: colorScheme == .dark, holder: holder)
            .overlay(alignment: .topTrailing) {
                HStack(spacing: 4) {
                    button("Fit".localized) { holder.view?.fitToContent() }
                    button("Reset".localized) {
                        OrbitRig.clearSavedCamera()
                        holder.view?.fitToContent()
                    }
                }
                .padding(6)
            }
    }

    private func button(_ label: String, action: @escaping () -> Void) -> some View {
        Button(label, action: action)
            .buttonStyle(.plain)
            .font(.caption2.weight(.medium))
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 5))
            .overlay(RoundedRectangle(cornerRadius: 5).strokeBorder(.quaternary))
            .help("Pinch or ⌘ + scroll to zoom.".localized)
    }
}

/// Lets the SwiftUI overlay buttons reach the AppKit view.
@MainActor
final class GraphHolder {
    weak var view: ContributionGraphView?
}

private struct ContributionGraphRepresentable: NSViewRepresentable {
    let grid: TokenBarCore.GridLayout
    let dark: Bool
    let holder: GraphHolder

    func makeNSView(context: Context) -> ContributionGraphView {
        let view = ContributionGraphView(frame: .zero)
        let scene = SCNScene()
        let rig = OrbitRig()
        scene.rootNode.addChildNode(rig.cameraNode)

        func addLight(_ type: SCNLight.LightType, intensity: CGFloat, from pos: SCNVector3?) {
            let light = SCNLight()
            light.type = type
            light.intensity = intensity * 1000
            let node = SCNNode()
            node.light = light
            if let pos {
                node.position = pos
                node.look(at: SCNVector3(0, 0, 0), up: SCNVector3(0, 1, 0), localFront: SCNVector3(0, 0, -1))
            }
            scene.rootNode.addChildNode(node)
        }
        addLight(.ambient, intensity: 0.7, from: nil)
        addLight(.directional, intensity: 0.8, from: SCNVector3(20, 30, 15))
        addLight(.directional, intensity: 0.25, from: SCNVector3(-15, 20, -10))

        view.scene = scene
        view.pointOfView = rig.cameraNode
        view.rig = rig
        view.backgroundColor = .clear
        view.antialiasingMode = .multisampling4X
        view.rendersContinuously = false // render on demand
        view.setupTooltip()
        holder.view = view

        installGrid(into: view, context: context)

        // Auto-fit on first appearance unless a saved camera was restored.
        view.needsInitialFit = !OrbitRig.hasSavedCamera
        return view
    }

    func updateNSView(_ view: ContributionGraphView, context: Context) {
        let signature = gridSignature
        if context.coordinator.signature != signature {
            installGrid(into: view, context: context)
        }
    }

    private var gridSignature: String {
        "\(grid.cols)|\(grid.maxTokens)|\(grid.cells.filter(\.active).count)|\(dark)"
    }

    private func installGrid(into view: ContributionGraphView, context: Context) {
        view.scene?.rootNode.childNode(withName: "grid", recursively: false)?.removeFromParentNode()
        let (node, mapping, corners) = buildGridNode(grid: grid, dark: dark)
        view.scene?.rootNode.addChildNode(node)
        view.cellByNodeName = mapping
        view.fitCorners = corners
        context.coordinator.signature = gridSignature
    }

    final class Coordinator {
        var signature = ""
    }

    func makeCoordinator() -> Coordinator { Coordinator() }
}
