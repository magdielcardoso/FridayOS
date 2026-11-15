// Friday's pointer: red HUD reticles drawn over the screen.
//
// Usage:
//   annotate <json_path>
//
// JSON payload:
//   {"seconds": 4, "rects": [{"x":100,"y":200,"w":220,"h":60,"label":"Salvar"}]}
//
// Coordinates are *global screen points* with the origin at the top-left,
// exactly like Core Graphics / `screencapture -R` / the `pixel_rect` values
// produced by the Vision OCR helper once divided by the display scale.
//
// Each rect becomes its own borderless, transparent, non-activating panel:
// 2px red border, 8px corner brackets and an optional Menlo label chip. The
// panels are click-through and never take focus, so the annotation can appear
// while the user is working in another app. Everything closes after `seconds`
// (default 4, capped at 15).
//
// Drawing an overlay needs no TCC permission: we never capture the screen and
// we never ask for Accessibility. This helper only needs AppKit.
//
// Build: `xcrun swiftc -O -o annotate annotate.swift` (done lazily by
// crates/friday-tools/src/swift.rs).

import AppKit
import Foundation

// MARK: - Payload

struct RectSpec: Decodable {
    let x: Double
    let y: Double
    let w: Double
    let h: Double
    let label: String?
}

struct Payload: Decodable {
    let seconds: Double?
    let rects: [RectSpec]
}

// MARK: - Errors

func fail(_ message: String) -> Never {
    let escaped = message
        .replacingOccurrences(of: "\\", with: "\\\\")
        .replacingOccurrences(of: "\"", with: "\\\"")
        .replacingOccurrences(of: "\n", with: "\\n")
    FileHandle.standardError.write(
        ("{\"ok\":false,\"error\":\"" + escaped + "\"}\n").data(using: .utf8)!
    )
    exit(1)
}

func emit(_ value: String) {
    FileHandle.standardOutput.write((value + "\n").data(using: .utf8)!)
}

func expandPath(_ raw: String) -> String {
    if raw.hasPrefix("~/") {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return home + String(raw.dropFirst(1))
    }
    return raw
}

// MARK: - Geometry

let primaryHeight = CGDisplayBounds(CGMainDisplayID()).height

/// Display bounds in Core Graphics (top-left origin) coordinates.
func cgBounds(for screen: NSScreen) -> CGRect {
    guard
        let number = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")]
            as? NSNumber
    else {
        return CGRect(origin: .zero, size: screen.frame.size)
    }
    return CGDisplayBounds(CGDirectDisplayID(number.uint32Value))
}

/// Picks the display that covers most of the rect, falling back to the main one.
func screenCovering(_ rect: CGRect) -> NSScreen {
    var best: NSScreen?
    var bestArea = 0.0
    for screen in NSScreen.screens {
        let intersection = rect.intersection(cgBounds(for: screen))
        guard !intersection.isNull, !intersection.isEmpty else { continue }
        let area = Double(intersection.width * intersection.height)
        if area > bestArea {
            bestArea = area
            best = screen
        }
    }
    return best ?? NSScreen.main ?? NSScreen.screens.first!
}

/// Converts a CG/top-left rect into AppKit's bottom-left global frame space.
func appKitFrame(for cgRect: CGRect) -> NSRect {
    NSRect(
        x: cgRect.origin.x,
        y: primaryHeight - cgRect.origin.y - cgRect.height,
        width: cgRect.width,
        height: cgRect.height
    )
}

// MARK: - Drawing

let borderPadding: CGFloat = 6
let labelBand: CGFloat = 18
let bracketLength: CGFloat = 8
let borderWidth: CGFloat = 2

func reticleColor() -> NSColor {
    NSColor(calibratedRed: 1.0, green: 0.23, blue: 0.19, alpha: 1.0)
}

enum LabelPlacement {
    case above
    case below
}

final class ReticleView: NSView {
    private let label: String?
    private let placement: LabelPlacement

    init(frame: NSRect, label: String?, placement: LabelPlacement) {
        self.label = label
        self.placement = placement
        super.init(frame: frame)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override var isOpaque: Bool { false }

    /// The actual annotated rect, in window-local bottom-left coordinates.
    /// The label band lives on the chosen side of the reticle.
    private var targetRect: NSRect {
        NSRect(
            x: borderPadding,
            y: placement == .above ? borderPadding : borderPadding + labelBand,
            width: max(0, bounds.width - 2 * borderPadding),
            height: max(0, bounds.height - 2 * borderPadding - labelBand)
        )
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current else { return }
        let rect = targetRect
        guard rect.width > 1, rect.height > 1 else { return }

        context.saveGraphicsState()
        reticleColor().setStroke()

        // 2px border, inset by half a line so it stays inside the panel.
        let border = rect.insetBy(dx: borderWidth / 2, dy: borderWidth / 2)
        let borderPath = NSBezierPath(rect: border)
        borderPath.lineWidth = borderWidth
        borderPath.stroke()

        // Corner brackets, slightly heavier, drawn on top of the border.
        let bracket = NSBezierPath()
        bracket.lineWidth = borderWidth
        let length = min(bracketLength, min(rect.width, rect.height) / 3)

        // bottom-left
        bracket.move(to: NSPoint(x: rect.minX, y: rect.minY + length))
        bracket.line(to: NSPoint(x: rect.minX, y: rect.minY))
        bracket.line(to: NSPoint(x: rect.minX + length, y: rect.minY))
        // bottom-right
        bracket.move(to: NSPoint(x: rect.maxX - length, y: rect.minY))
        bracket.line(to: NSPoint(x: rect.maxX, y: rect.minY))
        bracket.line(to: NSPoint(x: rect.maxX, y: rect.minY + length))
        // top-right
        bracket.move(to: NSPoint(x: rect.maxX, y: rect.maxY - length))
        bracket.line(to: NSPoint(x: rect.maxX, y: rect.maxY))
        bracket.line(to: NSPoint(x: rect.maxX - length, y: rect.maxY))
        // top-left
        bracket.move(to: NSPoint(x: rect.minX + length, y: rect.maxY))
        bracket.line(to: NSPoint(x: rect.minX, y: rect.maxY))
        bracket.line(to: NSPoint(x: rect.minX, y: rect.maxY - length))
        bracket.stroke()

        context.restoreGraphicsState()

        if let label, !label.isEmpty {
            drawLabel(label, target: rect)
        }
    }

    private func drawLabel(_ text: String, target rect: NSRect) {
        let font = NSFont(name: "Menlo", size: 10)
            ?? NSFont.monospacedSystemFont(ofSize: 10, weight: .regular)
        let color = reticleColor()
        let attributes: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: color,
        ]
        let attributed = NSAttributedString(string: text, attributes: attributes)
        let measured = attributed.size()

        let maxWidth = max(20, bounds.width - 2 * borderPadding)
        let chipWidth = min(measured.width + 10, maxWidth)
        let chipHeight = min(
            labelBand - 2,
            max(10, measured.height + 4)
        )
        let chipY: CGFloat
        switch placement {
        case .above:
            chipY = min(bounds.height - chipHeight - 1, rect.maxY + 2)
        case .below:
            chipY = max(1, rect.minY - chipHeight - 2)
        }
        let chip = NSRect(
            x: borderPadding,
            y: chipY,
            width: chipWidth,
            height: chipHeight
        )

        let chipPath = NSBezierPath(roundedRect: chip, xRadius: 3, yRadius: 3)
        NSColor(calibratedWhite: 0.05, alpha: 0.88).setFill()
        chipPath.fill()
        reticleColor().setStroke()
        chipPath.lineWidth = 1
        chipPath.stroke()

        let textRect = NSRect(
            x: chip.minX + 5,
            y: chip.midY - measured.height / 2,
            width: max(0, chip.width - 10),
            height: measured.height
        )
        attributed.draw(in: textRect)
    }
}

// MARK: - Panels

final class ReticleWindow: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

func makePanel(for spec: RectSpec) -> ReticleWindow? {
    guard
        spec.x.isFinite, spec.y.isFinite, spec.w.isFinite, spec.h.isFinite,
        spec.w > 0, spec.h > 0
    else {
        return nil
    }

    let target = CGRect(x: spec.x, y: spec.y, width: spec.w, height: spec.h)
    let screen = screenCovering(target)
    let display = cgBounds(for: screen)

    // Keep the label above the reticle unless the rect is glued to the top of
    // the display, in which case the chip flips below so the reticle stays
    // exactly on the requested coordinates.
    let placement: LabelPlacement =
        target.minY - borderPadding - labelBand >= display.minY ? .above : .below
    let bandAbove = placement == .above

    // Grow the panel so the reticle breathes and the label chip has room.
    let expanded = CGRect(
        x: target.origin.x - borderPadding,
        y: bandAbove
            ? target.origin.y - borderPadding - labelBand
            : target.origin.y - borderPadding,
        width: target.width + 2 * borderPadding,
        height: target.height + 2 * borderPadding + labelBand
    )

    let frame = appKitFrame(for: expanded)
    let panel = ReticleWindow(
        contentRect: frame,
        styleMask: [.borderless, .nonactivatingPanel],
        backing: .buffered,
        defer: false
    )

    panel.isOpaque = false
    panel.backgroundColor = .clear
    panel.hasShadow = false
    panel.level = .statusBar
    panel.ignoresMouseEvents = true
    panel.isMovable = false
    panel.isReleasedWhenClosed = false
    panel.hidesOnDeactivate = false
    panel.worksWhenModal = true
    panel.becomesKeyOnlyIfNeeded = true
    panel.collectionBehavior = [
        .canJoinAllSpaces,
        .fullScreenAuxiliary,
        .stationary,
        .ignoresCycle,
    ]
    panel.contentView = ReticleView(
        frame: NSRect(origin: .zero, size: frame.size),
        label: spec.label,
        placement: placement
    )
    panel.orderFrontRegardless()
    return panel
}

// MARK: - Main

let args = CommandLine.arguments
guard args.count >= 2 else {
    fail("uso: annotate <json_path>")
}

let path = expandPath(args[1])
guard FileManager.default.fileExists(atPath: path) else {
    fail("arquivo de payload não encontrado: \(path)")
}

let data: Data
do {
    data = try Data(contentsOf: URL(fileURLWithPath: path))
} catch {
    fail("não foi possível ler o payload: \(error.localizedDescription)")
}

let payload: Payload
do {
    payload = try JSONDecoder().decode(Payload.self, from: data)
} catch {
    fail("payload JSON inválido: \(error.localizedDescription)")
}

let seconds = min(15.0, max(0.5, payload.seconds ?? 4.0))
let capped = Array(payload.rects.prefix(64))
guard !capped.isEmpty else {
    fail("nenhum retângulo para anotar")
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
app.finishLaunching()

var panels: [ReticleWindow] = []
var drawn = 0
for spec in capped {
    if let panel = makePanel(for: spec) {
        panels.append(panel)
        drawn += 1
    }
}

guard drawn > 0 else {
    fail("todos os retângulos são inválidos (largura/altura precisam ser > 0)")
}

emit("{\"ok\":true,\"rects\":\(drawn)}")

func closeAll() {
    for panel in panels {
        panel.orderOut(nil)
        panel.close()
    }
    panels.removeAll()
}

// Keep the process alive exactly long enough for the reticles to be seen.
// `app.run()` is required for AppKit to actually put the panels on screen
// (a manually pumped run loop orders them but never displays them). Because a
// command-line agent has no bundle lifecycle, the timer exits the process
// directly instead of relying on `app.stop(nil)`.
Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in
    closeAll()
    exit(0)
}
DispatchQueue.main.asyncAfter(deadline: .now() + seconds + 0.5) {
    closeAll()
    exit(0)
}

app.run()
closeAll()
exit(0)
