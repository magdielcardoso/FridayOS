import CoreGraphics
import Foundation
import ImageIO

func drawIcon(size: Int, path: String) {
    let cs = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
    ctx.setFillColor(CGColor(red: 0.02, green: 0.01, blue: 0.01, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: size, height: size))
    let c = CGFloat(size) / 2
    for (radius, width, alpha) in [(0.40, 0.035, 1.0), (0.30, 0.020, 0.7), (0.20, 0.012, 0.5)] {
        ctx.setStrokeColor(CGColor(red: 1, green: 0.18, blue: 0.15, alpha: alpha))
        ctx.setLineWidth(CGFloat(size) * width)
        ctx.addArc(center: CGPoint(x: c, y: c), radius: CGFloat(size) * radius, startAngle: 0, endAngle: .pi * 2, clockwise: false)
        ctx.strokePath()
    }
    ctx.setFillColor(CGColor(red: 1, green: 0.18, blue: 0.15, alpha: 1))
    let core = CGFloat(size) * 0.055
    ctx.fillEllipse(in: CGRect(x: c - core, y: c - core, width: core * 2, height: core * 2))
    let url = URL(fileURLWithPath: path) as CFURL
    if let dest = CGImageDestinationCreateWithURL(url, "public.png" as CFString, 1, nil), let image = ctx.makeImage() {
        CGImageDestinationAddImage(dest, image, nil)
        CGImageDestinationFinalize(dest)
    }
}
let base = CommandLine.arguments[1]
drawIcon(size: 192, path: "\(base)/icon-192.png")
drawIcon(size: 512, path: "\(base)/icon-512.png")
drawIcon(size: 96, path: "\(base)/badge.png")
drawIcon(size: 180, path: "\(base)/apple-touch-icon.png")
print("icons ok")
