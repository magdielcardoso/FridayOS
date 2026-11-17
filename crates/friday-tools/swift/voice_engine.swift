// Friday's JARVIS-style audio foundation.
//
// A long-lived helper: it reads one command per line from stdin (or executes a
// single command given in argv) and writes one JSON object per line to stdout.
//
// Commands:
//   status                    engine / voice-processing state
//   capture-start <out.wav>   start a 16 kHz mono PCM16 tap on the input node
//   capture-stop              finalize the WAV and report its path
//   play <in.wav>             play a WAV through the engine's player node
//   quit                      tear down and exit
//
// Events on stdout (always one JSON object per line, always flushed):
//   {"event":"status","..."}   {"event":"started","..."}
//   {"event":"stopped","..."}  {"event":"play-start","..."}
//   {"event":"play-done","..."} {"event":"error","message":"..."}
//
// Build: xcrun swiftc -O -o <cache>/voice_engine voice_engine.swift
//
// macOS caveats:
// - AVAudioEngine voice processing (AEC) is available from macOS 10.15 and can
//   still fail on unsupported hardware; we degrade gracefully to a plain tap.
// - The player node is in the SAME engine graph as the voice-processed input.
//   That is what gives the echo canceller its far-end reference, so Friday's
//   own speech is subtracted from the captured microphone path.
// - TCC: the host app needs NSMicrophoneUsageDescription for capture.

import Foundation
import AVFoundation

// MARK: - JSON output

func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: []),
          let line = String(data: data, encoding: .utf8) else { return }
    print(line)
    fflush(stdout)
}

func emitError(_ message: String) {
    emit(["event": "error", "message": message])
}

// MARK: - Incremental linear resampler (device rate -> 16 kHz mono)

final class LinearResampler {
    private var phase: Double = 0
    private var prev: Float = 0
    private var started = false

    func reset() {
        phase = 0
        prev = 0
        started = false
    }

    /// Calls `sink` with freshly resampled 16 kHz mono chunks.
    func process(_ input: [Float], srcRate: Double, dstRate: Double = 16_000,
                 sink: ([Float]) -> Void) {
        guard srcRate > 0, !input.isEmpty else { return }
        let ratio = srcRate / dstRate
        var out: [Float] = []
        if abs(ratio - 1.0) < 1e-9 {
            out = input
        } else {
            out.reserveCapacity(Int(Double(input.count) / ratio) + 2)
            for cur in input {
                if !started {
                    prev = cur
                    started = true
                }
                while phase < 1.0 {
                    out.append(prev + (cur - prev) * Float(phase))
                    phase += ratio
                }
                phase -= 1.0
                prev = cur
            }
        }
        if !out.isEmpty {
            sink(out)
        }
    }
}

// MARK: - Engine host

final class VoiceEngineHost {
    let engine = AVAudioEngine()
    let player = AVAudioPlayerNode()
    /// Guards the capture state (tap callback runs on an audio thread).
    let lock = NSLock()
    let resampler = LinearResampler()

    var voiceProcessing = false
    var voiceProcessingNote = "not-initialized"
    var capturePath: String?
    var pcm = Data()
    var capturing = false
    var tapInstalled = false
    var started = false
    var streamHandle: FileHandle?

    init() {
        engine.attach(player)
        // Route the player into the main mixer. While voice processing is on,
        // the engine's output bus is the far-end reference used by the AEC, so
        // anything the player emits is cancelled from the microphone tap.
        engine.connect(player, to: engine.mainMixerNode, format: nil)
    }

    func prepare() throws {
        if !started {
            if #available(macOS 10.15, *) {
                do {
                    try engine.inputNode.setVoiceProcessingEnabled(true)
                    voiceProcessing = true
                    voiceProcessingNote = "enabled"
                } catch {
                    voiceProcessing = false
                    voiceProcessingNote = "unavailable: \(error.localizedDescription)"
                }
            } else {
                voiceProcessing = false
                voiceProcessingNote = "requires macOS 10.15+"
            }
        }
        if !engine.isRunning {
            try engine.start()
        }
        started = true
    }

    func stopAll() {
        lock.lock()
        capturing = false
        capturePath = nil
        pcm.removeAll(keepingCapacity: true)
        lock.unlock()
        if tapInstalled {
            engine.inputNode.removeTap(onBus: 0)
            tapInstalled = false
        }
        player.stop()
        streamHandle?.closeFile()
        streamHandle = nil
        if engine.isRunning {
            engine.stop()
        }
        started = false
    }
}

let host = VoiceEngineHost()

// MARK: - WAV writer

private func appendLE16(_ value: UInt16, to data: inout Data) {
    var v = value.littleEndian
    data.append(Data(bytes: &v, count: 2))
}

private func appendLE32(_ value: UInt32, to data: inout Data) {
    var v = value.littleEndian
    data.append(Data(bytes: &v, count: 4))
}

func writeWav(path: String, pcm: Data, sampleRate: UInt32 = 16_000) throws {
    let url = URL(fileURLWithPath: path)
    let parent = url.deletingLastPathComponent()
    try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)

    let channels: UInt16 = 1
    let bits: UInt16 = 16
    let byteRate = sampleRate * UInt32(channels) * UInt32(bits / 8)
    let blockAlign = channels * (bits / 8)

    var header = Data()
    header.append(contentsOf: "RIFF".utf8)
    appendLE32(UInt32(36 + pcm.count), to: &header)
    header.append(contentsOf: "WAVEfmt ".utf8)
    appendLE32(16, to: &header)
    appendLE16(1, to: &header)          // PCM
    appendLE16(channels, to: &header)
    appendLE32(sampleRate, to: &header)
    appendLE32(byteRate, to: &header)
    appendLE16(blockAlign, to: &header)
    appendLE16(bits, to: &header)
    header.append(contentsOf: "data".utf8)
    appendLE32(UInt32(pcm.count), to: &header)

    var out = header
    out.append(pcm)
    try out.write(to: url)
}

// MARK: - Commands

func cmdStatus() {
    emit([
        "event": "status",
        "available": true,
        "engine_running": host.engine.isRunning,
        "voice_processing": host.voiceProcessing,
        "voice_processing_note": host.voiceProcessingNote,
        "capturing": host.capturing,
        "sample_rate": 16_000,
    ])
}

func cmdCaptureStart(_ path: String) {
    guard !path.isEmpty else {
        emitError("capture-start: missing output path")
        return
    }
    do {
        try host.prepare()
        let input = host.engine.inputNode
        if host.tapInstalled {
            input.removeTap(onBus: 0)
            host.tapInstalled = false
        }

        host.lock.lock()
        host.pcm.removeAll(keepingCapacity: true)
        host.capturePath = path
        host.capturing = true
        host.resampler.reset()
        host.lock.unlock()

        // Use the node's own format and resample in the tap block. This avoids
        // relying on tap-level format conversion, which is finicky when voice
        // processing changes the input node's output format.
        input.installTap(onBus: 0, bufferSize: 4096, format: nil) { buffer, _ in
            guard host.capturing else { return }
            guard let channels = buffer.floatChannelData else { return }
            let frames = Int(buffer.frameLength)
            guard frames > 0 else { return }
            let channelCount = Int(buffer.format.channelCount)

            var mono = [Float](repeating: 0, count: frames)
            for c in 0..<channelCount {
                let ptr = channels[c]
                for i in 0..<frames {
                    mono[i] += ptr[i]
                }
            }
            if channelCount > 1 {
                let inv = 1.0 / Float(channelCount)
                for i in 0..<frames {
                    mono[i] *= inv
                }
            }

            host.lock.lock()
            host.resampler.process(mono, srcRate: buffer.format.sampleRate) { samples in
                var pcm = [Int16](repeating: 0, count: samples.count)
                for i in 0..<samples.count {
                    pcm[i] = Int16(max(-1.0, min(1.0, samples[i])) * 32_767.0)
                }
                host.pcm.append(Data(bytes: pcm, count: pcm.count * MemoryLayout<Int16>.size))
            }
            host.lock.unlock()
        }
        host.tapInstalled = true

        emit([
            "event": "started",
            "path": path,
            "sample_rate": 16_000,
            "voice_processing": host.voiceProcessing,
        ])
    } catch {
        emitError("capture-start: \(error.localizedDescription)")
    }
}

func cmdCaptureStop() {
    host.lock.lock()
    host.capturing = false
    let path = host.capturePath
    let pcm = host.pcm
    host.capturePath = nil
    host.pcm.removeAll(keepingCapacity: true)
    host.lock.unlock()

    if host.tapInstalled {
        host.engine.inputNode.removeTap(onBus: 0)
        host.tapInstalled = false
    }

    guard let path else {
        emitError("capture-stop: no active capture")
        return
    }
    do {
        try writeWav(path: path, pcm: pcm)
        let frames = pcm.count / MemoryLayout<Int16>.size
        let durationMs = UInt64(frames) * 1000 / 16_000
        emit([
            "event": "stopped",
            "path": path,
            "bytes": pcm.count,
            "frames": frames,
            "duration_ms": durationMs,
        ])
    } catch {
        emitError("capture-stop: \(error.localizedDescription)")
    }
}

// MARK: - Streaming capture (raw 16 kHz mono PCM16 appended to a file)

func cmdStreamStart(_ path: String) {
    guard !path.isEmpty else { emitError("stream-start: missing path"); return }
    do {
        try host.prepare()
        let input = host.engine.inputNode
        if host.tapInstalled { input.removeTap(onBus: 0); host.tapInstalled = false }
        let url = URL(fileURLWithPath: path)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(),
                                                withIntermediateDirectories: true)
        FileManager.default.createFile(atPath: path, contents: nil)
        host.streamHandle = try FileHandle(forWritingTo: url)
        host.lock.lock(); host.capturing = true; host.resampler.reset(); host.lock.unlock()

        input.installTap(onBus: 0, bufferSize: 4096, format: nil) { buffer, _ in
            guard host.capturing, let handle = host.streamHandle else { return }
            guard let channels = buffer.floatChannelData else { return }
            let frames = Int(buffer.frameLength)
            guard frames > 0 else { return }
            let channelCount = Int(buffer.format.channelCount)
            var mono = [Float](repeating: 0, count: frames)
            for c in 0..<channelCount {
                let ptr = channels[c]
                for i in 0..<frames { mono[i] += ptr[i] }
            }
            if channelCount > 1 {
                let inv = 1.0 / Float(channelCount)
                for i in 0..<frames { mono[i] *= inv }
            }
            host.resampler.process(mono, srcRate: buffer.format.sampleRate) { samples in
                var pcm = [Int16](repeating: 0, count: samples.count)
                for i in 0..<samples.count {
                    pcm[i] = Int16(max(-1.0, min(1.0, samples[i])) * 32_767.0)
                }
                // Throwing writer never raises an ObjC exception if the
                // handle was closed by a concurrent stream-stop.
                try? handle.write(contentsOf: Data(bytes: pcm, count: pcm.count * MemoryLayout<Int16>.size))
            }
        }
        host.tapInstalled = true
        emit(["event": "stream-started", "path": path,
              "voice_processing": host.voiceProcessing, "sample_rate": 16_000])
    } catch {
        emitError("stream-start: \(error.localizedDescription)")
    }
}

func cmdStreamStop() {
    host.lock.lock(); host.capturing = false; host.lock.unlock()
    if host.tapInstalled { host.engine.inputNode.removeTap(onBus: 0); host.tapInstalled = false }
    // Do not close from the command thread: an in-flight tap callback may
    // still hold the handle. Deallocation closes it safely on the next drain.
    host.streamHandle = nil
    emit(["event": "stream-stopped"])
}

private func stopPlayFlag() -> String {
    let home = NSHomeDirectory()
    return home + "/Library/Caches/FridayOS/voice-engine-stop-play"
}

func cmdStopPlay() {
    // Written by Rust without taking the engine lock, so it works even while
    // the wrapper is blocked waiting for `play-done`.
    let flag = stopPlayFlag()
    try? FileManager.default.createDirectory(
        at: URL(fileURLWithPath: (flag as NSString).deletingLastPathComponent),
        withIntermediateDirectories: true)
    FileManager.default.createFile(atPath: flag, contents: nil)
    emit(["event": "play-stopped"])
}

/// 20 ms linear fades at both ends, applied in the sample domain. This is a
/// deterministic form of gain automation and avoids the fragile AVAudioNode
/// parameter-automation API. It also guarantees the node ends at silence.
private func applyFade(_ buffer: AVAudioPCMBuffer, sampleRate: Double) {
    guard let channels = buffer.floatChannelData else { return }
    let frames = Int(buffer.frameLength)
    let fade = max(1, Int(sampleRate * 0.020))
    guard fade * 2 < frames else { return }
    let channelCount = Int(buffer.format.channelCount)
    for c in 0..<channelCount {
        let ptr = channels[c]
        for i in 0..<fade {
            let gain = Float(i) / Float(fade)
            ptr[i] *= gain
            ptr[frames - 1 - i] *= gain
        }
    }
}

func cmdPlay(_ path: String) {
    guard !path.isEmpty else {
        emitError("play: missing input path")
        return
    }
    do {
        // Clear a stale stop request left by a cancel with no active playback.
        try? FileManager.default.removeItem(atPath: stopPlayFlag())
        try host.prepare()
        let file = try AVAudioFile(forReading: URL(fileURLWithPath: path))
        guard file.length > 0 else {
            emitError("play: empty WAV")
            return
        }
        guard let buffer = AVAudioPCMBuffer(pcmFormat: file.processingFormat,
                                            frameCapacity: AVAudioFrameCount(file.length)) else {
            emitError("play: could not allocate buffer")
            return
        }
        try file.read(into: buffer)
        applyFade(buffer, sampleRate: file.processingFormat.sampleRate)

        host.player.stop()
        // AVAudioEngine's voice-processing path applies its own gain to the
        // far-end signal; a little headroom compensation keeps the voice at
        // the same level as afplay had before AEC was introduced.
        let gain = Float(min(max(Double(ProcessInfo.processInfo.environment["FRIDAY_AEC_GAIN"] ?? "1.8") ?? 1.8, 1.0), 4.0))
        host.player.volume = gain

        let doneLock = NSLock()
        var finished = false
        host.player.scheduleBuffer(buffer, at: nil, options: []) {
            doneLock.lock()
            finished = true
            doneLock.unlock()
        }
        host.player.play()
        emit(["event": "play-start", "path": path])

        let duration = Double(file.length) / max(1.0, file.processingFormat.sampleRate)
        let deadline = Date().addingTimeInterval(duration + 8.0)
        let stopFlag = stopPlayFlag()
        while Date() < deadline {
            if FileManager.default.fileExists(atPath: stopFlag) {
                try? FileManager.default.removeItem(atPath: stopFlag)
                host.player.stop()
                emit(["event": "play-stopped", "path": path])
                return
            }
            doneLock.lock()
            let done = finished
            doneLock.unlock()
            if done { break }
            RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.02))
        }

        doneLock.lock()
        let done = finished
        doneLock.unlock()
        host.player.stop() // never leave the node running
        if done {
            emit(["event": "play-done", "path": path])
        } else {
            emitError("play: timed out waiting for completion")
        }
    } catch {
        emitError("play: \(error.localizedDescription)")
    }
}

// MARK: - Command loop

func handle(_ line: String) -> Bool {
    let parts = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true)
        .map(String.init)
    guard let command = parts.first else { return true }
    let rest = parts.count > 1 ? parts[1].trimmingCharacters(in: .whitespaces) : ""

    switch command {
    case "status":
        cmdStatus()
    case "capture-start":
        cmdCaptureStart(rest)
    case "capture-stop":
        cmdCaptureStop()
    case "play":
        cmdPlay(rest)
    case "stream-start":
        cmdStreamStart(rest)
    case "stream-stop":
        cmdStreamStop()
    case "stop-play":
        cmdStopPlay()
    case "quit":
        host.stopAll()
        emit(["event": "quit"])
        return false
    default:
        emitError("unknown command: \(command)")
    }
    return true
}

let argv = Array(CommandLine.arguments.dropFirst())
if !argv.isEmpty {
    // One-shot mode for direct CLI use: e.g. `voice_engine status`.
    _ = handle(argv.joined(separator: " "))
} else {
    // Server mode used by the Rust wrapper. readLine blocks on stdin; when the
    // parent closes it we exit cleanly instead of spinning.
    while let line = readLine(strippingNewline: true) {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { continue }
        if !handle(trimmed) { break }
    }
}
