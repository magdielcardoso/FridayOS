// Friday's LOCAL streaming STT helper.
//
// A long-lived process: one command per line on stdin, one JSON object per line
// on stdout (always flushed). Unlike `speech transcribe` (whole WAV, batch) this
// keeps a continuous AVAudioEngine -> SFSpeechAudioBufferRecognitionRequest tap
// alive and emits partial transcripts as the user speaks, so Friday can do
// turn-taking without waiting for VAD endpoint + a network round trip.
//
// Commands:
//   start    start listening (idempotent; emits `started` when ready)
//   stop     cancel recognition, remove the tap, stop the engine, emit `stopped`
//   quit     stop, then exit
//
// Events on stdout:
//   {"event":"started"}
//   {"event":"partial","text":"..."}   whenever the best transcription changes
//   {"event":"final","text":"..."}     when the recognizer closes a phrase
//   {"event":"stopped"}
//   {"event":"error","message":"..."}
//
// Notes:
// - On-device only, pt-BR, partial results enabled. No file IO, no network.
// - NSError 1110 ("no speech detected") is a quiet timeout: it is NOT reported
//   as an error; we simply re-arm a fresh recognition task and stay listening.
// - Permission failures emit `error` and keep the process alive so the Rust
//   side can surface the message and the user can retry after granting access.
// - All `emit` calls are serialized by a lock; recognition callbacks run on
//   Apple's queue (not main), so we never rely on DispatchQueue.main here.
//
// Build: xcrun swiftc -O -o <cache>/stt_stream stt_stream.swift

import Foundation
import AVFoundation
import Speech

// MARK: - Output

private let emitLock = NSLock()

private func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: []),
          let line = String(data: data, encoding: .utf8) else { return }
    emitLock.lock()
    print(line)
    fflush(stdout)
    emitLock.unlock()
}

private func emitError(_ message: String) {
    emit(["event": "error", "message": message])
}

// MARK: - Permissions (pump the main run loop so TCC callbacks can land)

private func pumpMainRunLoop(until deadline: Date) {
    while Date() < deadline {
        RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
    }
}

private func requestSpeechAuthorization() -> SFSpeechRecognizerAuthorizationStatus {
    var status = SFSpeechRecognizer.authorizationStatus()
    if status != .notDetermined { return status }
    var finished = false
    SFSpeechRecognizer.requestAuthorization { newStatus in
        status = newStatus
        finished = true
    }
    let deadline = Date().addingTimeInterval(20)
    while !finished && Date() < deadline {
        RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
    }
    return status
}

private func requestMicrophoneAccess() -> Bool {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized:
        return true
    case .denied, .restricted:
        return false
    case .notDetermined:
        var granted = false
        var finished = false
        AVCaptureDevice.requestAccess(for: .audio) { ok in
            granted = ok
            finished = true
        }
        let deadline = Date().addingTimeInterval(20)
        while !finished && Date() < deadline {
            RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
        }
        return granted
    @unknown default:
        return false
    }
}

// MARK: - Host

private final class SttStreamHost {
    /// Guards request / task / lastText / running / tapInstalled.
    private let stateLock = NSLock()
    private let engine = AVAudioEngine()
    private var recognizer: SFSpeechRecognizer?
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private var tapInstalled = false
    private var running = false
    private var starting = false
    private var lastText = ""

    // MARK: start

    func start() {
        stateLock.lock()
        if running {
            stateLock.unlock()
            emit(["event": "started"]) // idempotent handshake
            return
        }
        if starting {
            stateLock.unlock()
            return
        }
        starting = true
        stateLock.unlock()

        guard requestSpeechAuthorization() == .authorized else {
            failStart("reconhecimento de fala não autorizado; autorize FridayOS em Ajustes do Sistema.")
            return
        }
        guard requestMicrophoneAccess() else {
            failStart("microfone não autorizado; autorize FridayOS em Ajustes do Sistema.")
            return
        }
        guard let recognizer = SFSpeechRecognizer(locale: Locale(identifier: "pt-BR")),
              recognizer.isAvailable, recognizer.supportsOnDeviceRecognition else {
            failStart("reconhecedor on-device pt-BR indisponível; baixe o idioma em Ajustes do Sistema > Teclado > Ditado.")
            return
        }

        stateLock.lock()
        self.recognizer = recognizer
        stateLock.unlock()

        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else {
            failStart("microfone padrão sem formato de áudio válido.")
            return
        }

        if tapInstalled {
            input.removeTap(onBus: 0)
            tapInstalled = false
        }
        // The tap only appends raw buffers; it never emits and never blocks.
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
            guard let self = self else { return }
            self.stateLock.lock()
            let req = self.request
            self.stateLock.unlock()
            req?.append(buffer)
        }
        tapInstalled = true

        engine.prepare()
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            tapInstalled = false
            failStart("falha ao iniciar o microfone: \(error.localizedDescription)")
            return
        }

        stateLock.lock()
        running = true
        starting = false
        stateLock.unlock()

        armTask()
        emit(["event": "started"])
    }

    private func failStart(_ message: String) {
        stateLock.lock()
        starting = false
        stateLock.unlock()
        emitError(message)
    }

    // MARK: recognition task

    /// Creates a fresh request + task and makes it the current one, so audio
    /// from the still-running engine tap feeds the next phrase.
    private func armTask() {
        stateLock.lock()
        let recognizer = self.recognizer
        let isRunning = self.running
        stateLock.unlock()
        guard let recognizer = recognizer, isRunning else { return }

        let req = SFSpeechAudioBufferRecognitionRequest()
        req.shouldReportPartialResults = true
        req.requiresOnDeviceRecognition = true
        if #available(macOS 13.0, *) { req.addsPunctuation = true }

        stateLock.lock()
        self.request = req
        self.lastText = ""
        stateLock.unlock()

        let created = recognizer.recognitionTask(with: req) { [weak self] result, error in
            guard let self = self else { return }
            self.handle(result: result, error: error, request: req)
        }
        stateLock.lock()
        self.task = created
        stateLock.unlock()
    }

    private func handle(result: SFSpeechRecognitionResult?, error: Error?,
                        request: SFSpeechAudioBufferRecognitionRequest) {
        stateLock.lock()
        // Ignore callbacks from a task we have already superseded.
        guard running, self.request === request else {
            stateLock.unlock()
            return
        }
        let isFinal = result?.isFinal ?? false
        let text = result?.bestTranscription.formattedString ?? lastText
        let changed = text != lastText
        if changed { lastText = text }
        stateLock.unlock()

        if let error = error {
            let ns = error as NSError
            if ns.code == 1110 {
                // "No speech detected": quiet timeout, re-arm silently.
                armTask()
                return
            }
            emitError("reconhecimento: \(error.localizedDescription)")
            armTask()
            return
        }

        if changed {
            emit(["event": "partial", "text": text])
        }
        if isFinal {
            emit(["event": "final", "text": text])
            // Stay ready for the next phrase with a fresh task/request.
            armTask()
        }
    }

    // MARK: stop

    func stop() {
        stateLock.lock()
        if !running && !starting {
            stateLock.unlock()
            emit(["event": "stopped"]) // idempotent handshake
            return
        }
        running = false
        starting = false
        let oldTask = self.task
        self.task = nil
        self.request = nil
        stateLock.unlock()

        oldTask?.cancel()
        if tapInstalled {
            engine.inputNode.removeTap(onBus: 0)
            tapInstalled = false
        }
        if engine.isRunning {
            engine.stop()
        }
        emit(["event": "stopped"])
    }
}

private let host = SttStreamHost()

// MARK: - Command loop

private func handle(_ line: String) -> Bool {
    switch line {
    case "start":
        host.start()
    case "stop":
        host.stop()
    case "quit":
        host.stop()
        return false
    default:
        emitError("comando desconhecido: \(line)")
    }
    return true
}

while let line = readLine(strippingNewline: true) {
    let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
    if trimmed.isEmpty { continue }
    if !handle(trimmed) { break }
}
