import CoreAudio
import Foundation

func propertyAddress(_ selector: AudioObjectPropertySelector, scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain)
}

func readDefaultOutput() -> AudioDeviceID {
    var address = propertyAddress(kAudioHardwarePropertyDefaultOutputDevice)
    var device = AudioDeviceID(0)
    var size = UInt32(MemoryLayout<AudioDeviceID>.size)
    AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &device)
    return device
}

func setDefaultOutput(_ device: AudioDeviceID) -> Bool {
    var address = propertyAddress(kAudioHardwarePropertyDefaultOutputDevice)
    var value = device
    let status = AudioObjectSetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, UInt32(MemoryLayout<AudioDeviceID>.size), &value)
    return status == noErr
}

func deviceName(_ device: AudioDeviceID) -> String {
    var address = propertyAddress(kAudioObjectPropertyName)
    var name: CFString = "" as CFString
    var size = UInt32(MemoryLayout<CFString>.size)
    let status = AudioObjectGetPropertyData(device, &address, 0, nil, &size, &name)
    return status == noErr ? (name as String) : "desconhecido"
}

func hasOutput(_ device: AudioDeviceID) -> Bool {
    var address = propertyAddress(kAudioDevicePropertyStreams, scope: kAudioObjectPropertyScopeOutput)
    var size: UInt32 = 0
    let status = AudioObjectGetPropertyDataSize(device, &address, 0, nil, &size)
    return status == noErr && size > 0
}

func jackConnected(_ device: AudioDeviceID) -> Bool? {
    // 'jack' — whether something is plugged into the analog jack.
    let selector = AudioObjectPropertySelector(0x6A61636B)
    var address = propertyAddress(selector)
    var value: UInt32 = 0
    var size = UInt32(MemoryLayout<UInt32>.size)
    let status = AudioObjectGetPropertyData(device, &address, 0, nil, &size, &value)
    return status == noErr ? value != 0 : nil
}

func outputVolume(_ device: AudioDeviceID) -> Float? {
    var address = propertyAddress(kAudioDevicePropertyVolumeScalar, scope: kAudioObjectPropertyScopeOutput)
    var value: Float32 = 0
    var size = UInt32(MemoryLayout<Float32>.size)
    let status = AudioObjectGetPropertyData(device, &address, 0, nil, &size, &value)
    return status == noErr ? value : nil
}

func allDevices() -> [AudioDeviceID] {
    var address = propertyAddress(kAudioHardwarePropertyDevices)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size) == noErr else { return [] }
    let count = Int(size) / MemoryLayout<AudioDeviceID>.size
    var devices = [AudioDeviceID](repeating: 0, count: count)
    guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &devices) == noErr else { return [] }
    return devices
}

func jsonEscape(_ value: String) -> String {
    value.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
}

let args = CommandLine.arguments.dropFirst()
let command = args.first ?? "list"
let current = readDefaultOutput()

switch command {
case "current":
    let jack = jackConnected(current).map { $0 ? "true" : "false" } ?? "null"
    let volume = outputVolume(current).map { String(format: "%.2f", $0) } ?? "null"
    print("{\"name\":\"\(jsonEscape(deviceName(current)))\",\"id\":\(current),\"jack\":\(jack),\"volume\":\(volume)}")
case "list":
    var rows: [String] = []
    for device in allDevices() where hasOutput(device) {
        let marker = device == current ? "true" : "false"
        let jack = jackConnected(device).map { $0 ? "true" : "false" } ?? "null"
        rows.append("{\"name\":\"\(jsonEscape(deviceName(device)))\",\"id\":\(device),\"default\":\(marker),\"jack\":\(jack)}")
    }
    print("[" + rows.joined(separator: ",") + "]")
case "set":
    let wanted = args.dropFirst().joined(separator: " ").trimmingCharacters(in: .whitespaces)
    guard !wanted.isEmpty else {
        print("{\"ok\":false,\"error\":\"informe o nome do dispositivo\"}")
        exit(1)
    }
    let matches = allDevices().filter { hasOutput($0) && deviceName($0).lowercased() == wanted.lowercased() }
    let fallback = allDevices().filter { hasOutput($0) && deviceName($0).lowercased().contains(wanted.lowercased()) }
    guard let target = matches.first ?? fallback.first else {
        print("{\"ok\":false,\"error\":\"dispositivo não encontrado: \(jsonEscape(wanted))\"}")
        exit(1)
    }
    if setDefaultOutput(target) {
        print("{\"ok\":true,\"name\":\"\(jsonEscape(deviceName(target)))\"}")
    } else {
        print("{\"ok\":false,\"error\":\"o sistema recusou a troca de saída\"}")
        exit(1)
    }
default:
    print("{\"ok\":false,\"error\":\"comando desconhecido: \(jsonEscape(command))\"}")
    exit(1)
}
