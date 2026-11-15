// Converts a PNG/JPG into dithered ASCII art.
// Usage: xcrun swift scripts/ascii.swift <input> <output.txt> <width>
import Foundation
import CoreGraphics
import ImageIO

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write("uso: ascii.swift <entrada> <saida> [largura]\n".data(using: .utf8)!)
    exit(1)
}
let inputPath = args[1]
let outputPath = args[2]
let width = args.count > 3 ? Int(args[3]) ?? 118 : 118
// Optional crop, in fractions of the original height: top, bottom.
let cropTop = args.count > 4 ? Double(args[4]) ?? 0.0 : 0.0
let cropBottom = args.count > 5 ? Double(args[5]) ?? 1.0 : 1.0
let cropLeft = args.count > 6 ? Double(args[6]) ?? 0.0 : 0.0
let cropRight = args.count > 7 ? Double(args[7]) ?? 1.0 : 1.0

guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: inputPath) as CFURL, nil),
      let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else {
    FileHandle.standardError.write("não consegui abrir \(inputPath)\n".data(using: .utf8)!)
    exit(1)
}

let cropHeight = max(1.0, cropBottom - cropTop)
let pixelHeight = Double(image.height) * cropHeight
let pixelWidth = Double(image.width) * max(0.05, cropRight - cropLeft)
let aspect = pixelHeight / pixelWidth
let height = max(8, Int((Double(width) * aspect * 0.52).rounded()))

// High-quality downsample into a grayscale buffer.
var pixels = [UInt8](repeating: 0, count: width * height)
let colorSpace = CGColorSpaceCreateDeviceGray()
pixels.withUnsafeMutableBytes { raw in
    guard let ctx = CGContext(
        data: raw.baseAddress,
        width: width,
        height: height,
        bitsPerComponent: 8,
        bytesPerRow: width,
        space: colorSpace,
        bitmapInfo: CGImageAlphaInfo.none.rawValue
    ) else { return }
    ctx.interpolationQuality = .high
    let sourceRect = CGRect(
        x: Double(image.width) * cropLeft,
        y: Double(image.height) * cropTop,
        width: pixelWidth,
        height: pixelHeight
    )
    guard let cropped = image.cropping(to: sourceRect) else { return }
    ctx.draw(cropped, in: CGRect(x: 0, y: 0, width: width, height: height))
}

let bayer: [[Int]] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
]
let ramp = Array(" .:-=+*#%@")
let levels = Double(ramp.count - 1)

var lines: [String] = []
for y in 0..<height {
    var line = ""
    for x in 0..<width {
        var raw = Double(pixels[y * width + x]) / 255.0
        // Vignette: fade everything outside the face to black, like the
        // reference illustration.
        let vx = (Double(x) / Double(width) - 0.5) / 0.58
        let vy = (Double(y) / Double(height) - 0.42) / 0.62
        let radius = (vx * vx + vy * vy).squareRoot()
        let mask = min(1.0, max(0.0, (1.05 - radius) / 0.25))
        raw *= mask
        // Contrast: crush the background to black, keep the face lit.
        let value = min(1.0, max(0.0, (raw - 0.16) * 1.6))
        let threshold = (Double(bayer[y % 4][x % 4]) / 16.0 - 0.5) * 0.12
        let adjusted = min(1.0, max(0.0, value + threshold))
        let index = Int((adjusted * levels).rounded())
        line.append(ramp[index])
    }
    lines.append(line.replacingOccurrences(of: " +$", with: "", options: .regularExpression))
}

try lines.joined(separator: "\n").write(toFile: outputPath, atomically: true, encoding: .utf8)
print("ascii: \(width)x\(height) -> \(outputPath)")
