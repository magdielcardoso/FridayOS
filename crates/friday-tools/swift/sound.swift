// Friday's tiny UI sound designer.
//
// Usage:
//   sound play <name>   name = wake | confirm | error | thinking | done
//
// Synthesizes short, subtle tones locally with AVAudioEngine + AVAudioPlayerNode.
// No network, no files. Prints one JSON line: {"done":true} (or
// {"done":false,"error":"..."} on failure) and exits.
//
// Build: xcrun swiftc -O -o <cache>/sound sound.swift

import Foundation
import AVFoundation

func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: []),
          let line = String(data: data, encoding: .utf8) else { return }
    print(line)
    fflush(stdout)
}

/// A musical segment: frequency in Hz, duration in seconds, and a per-note gain
/// multiplier relative to the sound's master volume.
private typealias Segment = (freq: Double, duration: Double, gain: Double)

private let sounds: [String: (segments: [Segment], volume: Float)] = [
    // Soft two-note rising chime.
    "wake": ([
        (659.25, 0.055, 0.8), (0, 0.012, 0), (987.77, 0.070, 1.0),
    ], 0.10),
    // Crisp two-tone acknowledgement.
    "confirm": ([
        (880.0, 0.035, 1.0), (0, 0.008, 0), (1174.66, 0.045, 0.9),
    ], 0.09),
    // Low descending pair — urgent but not harsh.
    "error": ([
        (392.0, 0.060, 0.9), (0, 0.010, 0), (261.63, 0.080, 1.0),
    ], 0.09),
    // Three gentle ticks, like a quiet "working on it".
    "thinking": ([
        (523.25, 0.030, 0.7), (0, 0.018, 0),
        (523.25, 0.030, 0.6), (0, 0.018, 0),
        (523.25, 0.030, 0.5),
    ], 0.07),
    // Small major arpeggio — task finished.
    "done": ([
        (523.25, 0.050, 0.8), (0, 0.008, 0),
        (659.25, 0.050, 0.9), (0, 0.008, 0),
        (783.99, 0.060, 1.0),
    ], 0.09),
]

private func renderBuffer(_ spec: (segments: [Segment], volume: Float),
                          format: AVAudioFormat) -> AVAudioPCMBuffer? {
    // Match the mixer's real channel count: scheduling a mono buffer into a
    // stereo mixer crashed with _outputFormat.channelCount == buffer.format...
    let sampleRate = format.sampleRate
    let channels = Int(format.channelCount)
    let totalDuration = spec.segments.reduce(0.0) { $0 + $1.duration }
    let frames = Int((totalDuration * sampleRate).rounded(.up))
    guard frames > 0, let pcmFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                                    sampleRate: sampleRate,
                                                    channels: AVAudioChannelCount(channels),
                                                    interleaved: false),
          let buffer = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: AVAudioFrameCount(frames)) else {
        return nil
    }
    buffer.frameLength = AVAudioFrameCount(frames)
    guard let data = buffer.floatChannelData else { return nil }

    var cursor = 0
    let twoPi = 2.0 * Double.pi
    for segment in spec.segments {
        let segmentFrames = Int((segment.duration * sampleRate).rounded(.up))
        // A 4 ms attack and a 10 ms release keep every note free of clicks.
        let attack = max(1, Int(sampleRate * 0.004))
        let release = max(1, Int(sampleRate * 0.010))
        for i in 0..<segmentFrames {
            let index = cursor + i
            if index >= frames { break }
            if segment.freq <= 0 || segment.gain <= 0 {
                for channel in 0..<channels { data[channel][index] = 0 }
                continue
            }
            let envelope: Double
            if i < attack {
                envelope = Double(i) / Double(attack)
            } else if i >= segmentFrames - release {
                envelope = Double(max(0, segmentFrames - 1 - i)) / Double(release)
            } else {
                envelope = 1.0
            }
            let t = Double(i) / sampleRate
            let wave = sin(twoPi * segment.freq * t)
            let sample = Float(wave * envelope * segment.gain * Double(spec.volume))
            for channel in 0..<channels { data[channel][index] = sample }
        }
        cursor += segmentFrames
    }
    return buffer
}

let args = Array(CommandLine.arguments.dropFirst())
guard args.count >= 2, args[0] == "play", let spec = sounds[args[1]] else {
    emit(["done": false, "error": "usage: sound play <wake|confirm|error|thinking|done>"])
    exit(1)
}

do {
    let engine = AVAudioEngine()
    let player = AVAudioPlayerNode()
    engine.attach(player)
    engine.connect(player, to: engine.mainMixerNode, format: nil)
    try engine.start()

    guard let buffer = renderBuffer(spec, format: engine.mainMixerNode.outputFormat(forBus: 0)) else {
        emit(["done": false, "error": "could not synthesize tone"])
        exit(1)
    }
    player.scheduleBuffer(buffer, at: nil, options: [])
    player.play()

    // Bounded wait: if the completion handler never fires, do not hang.
    let deadline = Date().addingTimeInterval(2.0)
    while player.isPlaying && Date() < deadline {
        RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.01))
    }
    player.stop()
    engine.stop()
    emit(["done": true])
} catch {
    emit(["done": false, "error": "\(error.localizedDescription)"])
    exit(1)
}
