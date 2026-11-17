// Friday's on-device ears.
//
// Usage:
//   speech auth              request speech-recognition authorization, print status
//   speech transcribe <wav>  transcribe an audio file on-device (Siri engine)
//   speech speak <text>      speak with the system/Siri voice, print progress
//   speech listen <seconds>  transcribe microphone audio, print JSON lines:
//                            {"text": "...", "final": false}
//                            exits when the time limit is reached
//
// All output is one JSON object per line on stdout. Failures print
// {"error": "..."} and exit non-zero, so the Rust layer can surface a clear
// message instead of parsing log noise.
//
// Build: xcrun swiftc -O -o <cache>/speech speech.swift

import Foundation
import Speech
import AVFoundation

// Keeps the synthesizer delegate alive (AVSpeechSynthesizer.delegate is weak).
final class SpeechDelegate: NSObject, AVSpeechSynthesizerDelegate {
    var onEvent: ([String: Any]) -> Void
    var finished = false
    init(onEvent: @escaping ([String: Any]) -> Void) {
        self.onEvent = onEvent
    }
    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer,
                           willSpeakRangeOfSpeechString characterRange: NSRange,
                           utterance: AVSpeechUtterance) {
        let total = utterance.speechString.utf16.count
        guard total > 0 else { return }
        onEvent(["progress": Double(characterRange.location + characterRange.length) / Double(total)])
    }
    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer,
                           didFinish utterance: AVSpeechUtterance) { finished = true }
    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer,
                           didCancel utterance: AVSpeechUtterance) { finished = true }
}

func selectedVoice() -> AVSpeechSynthesisVoice? {
    let environment = ProcessInfo.processInfo.environment
    if let identifier = environment["FRIDAY_VOICE_ID"],
       let voice = AVSpeechSynthesisVoice(identifier: identifier) {
        return voice
    }
    let portuguese = AVSpeechSynthesisVoice.speechVoices().filter { $0.language.hasPrefix("pt-BR") }
    return portuguese.first { $0.quality == .premium }
        ?? portuguese.first { $0.quality == .enhanced }
        ?? portuguese.first { $0.name == "Luciana" }
        ?? portuguese.first
}

// MARK: - Output helpers

func emit(_ object: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: object, options: []),
       let line = String(data: data, encoding: .utf8) {
        print(line)
        fflush(stdout)
    }
}

func fail(_ message: String, code: Int32 = 1) -> Never {
    emit(["error": message])
    exit(code)
}

// MARK: - Permission helpers

func speechStatusName(_ status: SFSpeechRecognizerAuthorizationStatus) -> String {
    switch status {
    case .authorized: return "authorized"
    case .denied: return "denied"
    case .restricted: return "restricted"
    case .notDetermined: return "notDetermined"
    @unknown default: return "unknown"
    }
}

func microphoneStatusName(_ status: AVAuthorizationStatus) -> String {
    switch status {
    case .authorized: return "authorized"
    case .denied: return "denied"
    case .restricted: return "restricted"
    case .notDetermined: return "notDetermined"
    @unknown default: return "unknown"
    }
}

/// Pumps the main run loop so TCC callbacks can land.
func pumpMainRunLoop(until deadline: Date) {
    while Date() < deadline {
        RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
    }
}

func requestSpeechAuthorization() -> SFSpeechRecognizerAuthorizationStatus {
    var status = SFSpeechRecognizer.authorizationStatus()
    if status != .notDetermined {
        return status
    }
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

func requestMicrophoneAccess() -> Bool {
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

// MARK: - Arguments

let arguments = CommandLine.arguments
let command = arguments.count > 1 ? arguments[1] : "listen"

// MARK: - On-device file transcription (same engine Siri/Dictation uses)

if command == "transcribe" {
    guard arguments.count > 2 else { fail("use: speech transcribe <arquivo.wav>", code: 64) }
    let path = arguments[2]
    guard FileManager.default.fileExists(atPath: path) else {
        fail("arquivo de áudio não existe", code: 65)
    }
    guard requestSpeechAuthorization() == .authorized else {
        fail("reconhecimento de fala negado; autorize FridayOS em Ajustes do Sistema.", code: 3)
    }
    guard let recognizer = SFSpeechRecognizer(locale: Locale(identifier: "pt-BR")),
          recognizer.isAvailable, recognizer.supportsOnDeviceRecognition else {
        fail("reconhecedor on-device pt-BR indisponível", code: 7)
    }
    let request = SFSpeechURLRecognitionRequest(url: URL(fileURLWithPath: path))
    request.requiresOnDeviceRecognition = true
    request.shouldReportPartialResults = false
    if #available(macOS 13.0, *) { request.addsPunctuation = true }

    var text = ""
    var done = false
    let task = recognizer.recognitionTask(with: request) { result, error in
        if let result = result {
            text = result.bestTranscription.formattedString
            if result.isFinal { done = true }
        }
        if error != nil { done = true }
    }
    let limit = Date().addingTimeInterval(40)
    while !done && Date() < limit {
        RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
    }
    task.cancel()
    let clean = text.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !clean.isEmpty else { fail("nenhuma fala reconhecida no áudio", code: 10) }
    emit(["text": clean, "final": true])
    exit(0)
}

// MARK: - Native speech synthesis

if command == "speak" || command == "speak-prosody" {
    let environment = ProcessInfo.processInfo.environment
    var rate = Float(environment["FRIDAY_SPEECH_RATE"] ?? "0.5") ?? 0.5
    var pitch: Float = 1.0
    var words = Array(arguments.dropFirst(2))
    if command == "speak-prosody" && words.count >= 2 {
        rate = Float(words[0]) ?? rate
        pitch = Float(words[1]) ?? pitch
        words = Array(words.dropFirst(2))
    }
    let text = words.joined(separator: " ").trimmingCharacters(in: .whitespacesAndNewlines)
    guard !text.isEmpty else { fail("texto vazio", code: 64) }
    let utterance = AVSpeechUtterance(string: text)
    utterance.voice = selectedVoice()
    // AVSpeechUtterance's nominal rate is 0.5; multipliers from the app stay
    // in a subtle 0.75..1.35 band around it.
    utterance.rate = min(max(rate * 0.5, 0.28), 0.72)
    utterance.pitchMultiplier = min(max(pitch, 0.85), 1.18)
    utterance.volume = 1.0

    let synthesizer = AVSpeechSynthesizer()
    let delegate = SpeechDelegate { event in emit(event) }
    synthesizer.delegate = delegate
    synthesizer.speak(utterance)
    let limit = Date().addingTimeInterval(min(120.0, 10.0 + Double(text.count) * 0.25))
    while !delegate.finished && Date() < limit {
        RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.02))
    }
    synthesizer.stopSpeaking(at: .immediate)
    emit(["done": true])
    exit(delegate.finished ? 0 : 11)
}

if command == "auth" {
    let speech = requestSpeechAuthorization()
    let microphone = AVCaptureDevice.authorizationStatus(for: .audio)
    emit([
        "speech": speechStatusName(speech),
        "microphone": microphoneStatusName(microphone),
        "on_device_supported": SFSpeechRecognizer()?.supportsOnDeviceRecognition ?? false,
    ])
    exit(speech == .authorized ? 0 : 2)
}

guard command == "listen" else {
    fail("comando desconhecido: \(command). Use `auth` ou `listen <segundos>`.", code: 64)
}

let requestedSeconds = arguments.count > 2 ? (Double(arguments[2]) ?? 8.0) : 8.0
let seconds = max(1.0, min(120.0, requestedSeconds))
let deadline = Date().addingTimeInterval(seconds)

// MARK: - Permissions

let speechStatus = requestSpeechAuthorization()
guard speechStatus == .authorized else {
    fail(
        "reconhecimento de fala não autorizado (\(speechStatusName(speechStatus))); " +
        "autorize FridayOS em Ajustes do Sistema > Privacidade e Segurança > Reconhecimento de Fala.",
        code: 3
    )
}

guard requestMicrophoneAccess() else {
    fail(
        "microfone negado; autorize FridayOS em Ajustes do Sistema > Privacidade e Segurança > Microfone.",
        code: 4
    )
}

// MARK: - Recognizer

var recognizer = SFSpeechRecognizer(locale: Locale(identifier: "pt-BR"))
if recognizer == nil {
    recognizer = SFSpeechRecognizer(locale: Locale.current)
}
guard let recognizer = recognizer else {
    fail("nenhum reconhecedor de fala disponível neste Mac.", code: 5)
}
guard recognizer.isAvailable else {
    fail("o reconhecedor de fala está temporariamente indisponível.", code: 6)
}
guard recognizer.supportsOnDeviceRecognition else {
    fail(
        "reconhecimento on-device indisponível; baixe o idioma em Ajustes do Sistema > " +
        "Teclado > Ditado (ou ative o Siri) e tente de novo.",
        code: 7
    )
}

let request = SFSpeechAudioBufferRecognitionRequest()
request.shouldReportPartialResults = true
request.requiresOnDeviceRecognition = true
if #available(macOS 13.0, *) {
    request.addsPunctuation = true
}

let audioEngine = AVAudioEngine()
let inputNode = audioEngine.inputNode
let recordingFormat = inputNode.outputFormat(forBus: 0)
if recordingFormat.sampleRate <= 0 || recordingFormat.channelCount == 0 {
    fail("o microfone padrão não expôs um formato de áudio válido.", code: 8)
}

inputNode.installTap(onBus: 0, bufferSize: 1024, format: recordingFormat) { buffer, _ in
    request.append(buffer)
}

audioEngine.prepare()
do {
    try audioEngine.start()
} catch {
    inputNode.removeTap(onBus: 0)
    fail("falha ao iniciar o microfone: \(error.localizedDescription)", code: 9)
}

// MARK: - Recognition loop

var lastText = ""
var finished = false
var finalEmitted = false

let task = recognizer.recognitionTask(with: request) { result, error in
    DispatchQueue.main.async {
        if let result = result {
            let text = result.bestTranscription.formattedString
            if !text.isEmpty {
                lastText = text
            }
            emit(["text": text, "final": result.isFinal])
            if result.isFinal {
                finished = true
                finalEmitted = true
            }
        }
        if let error = error {
            let nsError = error as NSError
            // 1110 ("no speech detected") is an expected quiet-timeout, not a failure.
            if nsError.code != 1110 {
                emit(["error": "reconhecimento: \(error.localizedDescription)"])
                finished = true
            }
        }
    }
}

while Date() < deadline && !finished {
    RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.1))
}

audioEngine.stop()
inputNode.removeTap(onBus: 0)
request.endAudio()
task.cancel()

if !finalEmitted && !lastText.isEmpty {
    emit(["text": lastText, "final": true])
}

exit(0)
