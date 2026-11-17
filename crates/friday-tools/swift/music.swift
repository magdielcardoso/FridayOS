// FridayOS ambient music bed. Local only: either a user-supplied audio file
// (FRIDAY_MUSIC_FILE) or a quiet procedural pad synthesized here.
//
// Protocol: one JSON command per line on stdin, one JSON event per line on
// stdout. Commands: start [path], stop, duck on|off, status, quit.

import AVFoundation
import Foundation

func emit(_ object: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: object),
       let line = String(data: data, encoding: .utf8) {
        print(line)
        fflush(stdout)
    }
}

private let engine = AVAudioEngine()
private let player = AVAudioPlayerNode()
private var filePlayer: AVAudioPlayer?
private var running = false
private var ducked = false
private var attached = false

private let baseVolume: Float = 0.045
private let duckFactor: Float = 0.18

private func padBuffer() -> AVAudioPCMBuffer? {
    let format = engine.mainMixerNode.outputFormat(forBus: 0)
    let rate = format.sampleRate
    let channels = Int(format.channelCount)
    let seconds = 8.0
    let frames = Int(rate * seconds)
    guard let pcm = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                  sampleRate: rate,
                                  channels: AVAudioChannelCount(channels),
                                  interleaved: false),
          let buffer = AVAudioPCMBuffer(pcmFormat: pcm, frameCapacity: AVAudioFrameCount(frames)) else {
        return nil
    }
    buffer.frameLength = AVAudioFrameCount(frames)
    guard let data = buffer.floatChannelData else { return nil }
    // Four soft voices, very quiet, with a slow amplitude swell so the loop
    // has no audible seam.
    let notes: [Double] = [110.0, 164.81, 220.0, 329.63]
    for i in 0..<frames {
        let t = Double(i) / rate
        let swell = 0.7 + 0.3 * sin(2.0 * Double.pi * t / seconds)
        var value = 0.0
        for (index, note) in notes.enumerated() {
            let phase = 2.0 * Double.pi * note * t
            let weight = 1.0 / Double(index + 2)
            value += sin(phase) * weight
            value += sin(phase * 2.0) * weight * 0.12
        }
        let sample = Float(value * 0.06 * swell)
        for channel in 0..<channels { data[channel][i] = sample }
    }
    return buffer
}

private func applyVolume() {
    player.volume = ducked ? baseVolume * duckFactor : baseVolume
    filePlayer?.volume = ducked ? 0.05 : 0.28
}

private func start(_ path: String?) {
    stop()
    if let path = path, FileManager.default.fileExists(atPath: path) {
        do {
            filePlayer = try AVAudioPlayer(contentsOf: URL(fileURLWithPath: path))
            filePlayer?.numberOfLoops = -1
            filePlayer?.prepareToPlay()
            filePlayer?.play()
            applyVolume()
            running = true
            emit(["event": "started", "source": "file", "path": path])
            return
        } catch {
            emit(["event": "error", "message": "música: \(error.localizedDescription)"])
        }
    }
    guard let buffer = padBuffer() else {
        emit(["event": "error", "message": "música: não foi possível gerar o pad"])
        return
    }
    if !attached {
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: nil)
        attached = true
    }
    do {
        try engine.start()
        player.scheduleBuffer(buffer, at: nil, options: [.loops], completionHandler: nil)
        player.play()
        applyVolume()
        running = true
        emit(["event": "started", "source": "pad"])
    } catch {
        emit(["event": "error", "message": "música: \(error.localizedDescription)"])
    }
}

private func stop() {
    player.stop()
    if attached {
        engine.detach(player)
        attached = false
    }
    engine.stop()
    filePlayer?.stop()
    filePlayer = nil
    running = false
    emit(["event": "stopped"])
}

while let line = readLine(strippingNewline: true) {
    let parts = line.split(separator: " ").map(String.init)
    guard let command = parts.first else { continue }
    switch command {
    case "start":
        start(parts.count > 1 ? parts[1] : nil)
    case "stop":
        stop()
    case "duck":
        ducked = parts.count > 1 && parts[1] == "on"
        applyVolume()
        emit(["event": "duck", "on": ducked])
    case "status":
        emit(["event": "status", "running": running, "ducked": ducked])
    case "quit":
        stop()
        emit(["event": "quit"])
        exit(0)
    default:
        emit(["event": "error", "message": "comando desconhecido: \(command)"])
    }
}
