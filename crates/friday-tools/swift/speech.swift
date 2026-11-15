// Friday's on-device ears.
//
// Usage:
//   speech auth              request speech-recognition authorization, print status
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
