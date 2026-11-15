// Friday's eyes: a tiny Vision.framework helper.
//
// Subcommands:
//   vision ocr <path>       -> JSON array of recognized text blocks
//   vision has_text <path>  -> 1 if any text was found, 0 otherwise
//
// Coordinates are normalized (0...1) and keep Vision's origin: bottom-left,
// y up. The Rust side flips them to top-left before returning them to the
// model, because that is what "click at this point" needs.
//
// Building: `xcrun swiftc -O -o vision vision.swift` (done lazily by
// crates/friday-tools/src/swift.rs).

import Foundation
import Vision
import ImageIO

struct Block: Codable {
    let text: String
    let confidence: Double
    let x: Double
    let y: Double
    let width: Double
    let height: Double
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(("vision: " + message + "\n").data(using: .utf8)!)
    exit(1)
}

func expandPath(_ raw: String) -> String {
    if raw.hasPrefix("~/") {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return home + String(raw.dropFirst(1))
    }
    return raw
}

func loadImage(_ path: String) -> CGImage {
    let expanded = expandPath(path)
    let url = URL(fileURLWithPath: expanded)
    guard FileManager.default.fileExists(atPath: url.path) else {
        fail("arquivo não encontrado: \(url.path)")
    }
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
    else {
        fail("não foi possível decodificar a imagem: \(url.path)")
    }
    return image
}

func makeRequest(accurate: Bool) -> VNRecognizeTextRequest {
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = accurate ? .accurate : .fast
    request.usesLanguageCorrection = true
    request.recognitionLanguages = ["en-US", "pt-BR"]
    if #available(macOS 13.0, *) {
        request.automaticallyDetectsLanguage = true
    }
    return request
}

/// Reads top-to-bottom, left-to-right. Vision reports y with the origin at the
/// bottom-left, so "higher on screen" means a *larger* midY.
func ordered(_ blocks: [Block]) -> [Block] {
    let sorted = blocks.sorted {
        if abs($0.y - $1.y) > 0.0001 { return $0.y > $1.y }
        return $0.x < $1.x
    }
    var rows: [Block] = []
    var current: [Block] = []
    var baseline = 0.0

    func flush() {
        if current.isEmpty { return }
        rows.append(contentsOf: current.sorted { $0.x < $1.x })
        current.removeAll()
    }

    for block in sorted {
        let midY = block.y + block.height / 2
        if current.isEmpty {
            current.append(block)
            baseline = midY
        } else if abs(midY - baseline) <= 0.012 {
            current.append(block)
        } else {
            flush()
            current.append(block)
            baseline = midY
        }
    }
    flush()
    return rows
}

func recognize(_ path: String, accurate: Bool) -> [Block] {
    let image = loadImage(path)
    let request = makeRequest(accurate: accurate)
    let handler = VNImageRequestHandler(cgImage: image, options: [:])
    do {
        try handler.perform([request])
    } catch {
        fail("Vision falhou: \(error.localizedDescription)")
    }

    let observations = request.results ?? []
    var blocks: [Block] = []
    for observation in observations {
        guard let candidate = observation.topCandidates(1).first else { continue }
        let text = candidate.string.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.isEmpty { continue }
        let box = observation.boundingBox
        blocks.append(
            Block(
                text: text,
                confidence: Double(candidate.confidence),
                x: Double(box.origin.x),
                y: Double(box.origin.y),
                width: Double(box.size.width),
                height: Double(box.size.height)
            )
        )
    }
    return ordered(blocks)
}

func runOCR(_ path: String) -> Never {
    let blocks = recognize(path, accurate: true)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.withoutEscapingSlashes]
    do {
        let data = try encoder.encode(blocks)
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write("\n".data(using: .utf8)!)
    } catch {
        fail("não foi possível serializar o resultado: \(error.localizedDescription)")
    }
    exit(0)
}

func runHasText(_ path: String) -> Never {
    let blocks = recognize(path, accurate: false)
    let hasText = blocks.contains { !$0.text.isEmpty }
    print(hasText ? "1" : "0")
    exit(0)
}

let args = CommandLine.arguments
guard args.count >= 3 else {
    fail("uso: vision ocr <path> | vision has_text <path>")
}

switch args[1] {
case "ocr":
    runOCR(args[2])
case "has_text":
    runHasText(args[2])
default:
    fail("subcomando desconhecido: \(args[1])")
}
