// Friday's input helper: trusted state, typing, keys, mouse and scrolling.
//
// Every subcommand prints one line of JSON:
//   {"ok":true,...}  or  {"ok":false,"error":"..."}
//
// Coordinates are CGEvent global display coordinates: origin at the TOP-LEFT
// of the main display, Y growing downwards, measured in points (not pixels).
//
// Posting synthetic events requires the Accessibility permission for the
// calling app (FridayOS, or the terminal that launched it).

import ApplicationServices
import Foundation

let accessibilityHelp =
    "Sem permissão de Acessibilidade. Abra Ajustes do Sistema > Privacidade e Segurança > "
    + "Acessibilidade e habilite o FridayOS (ou o app/terminal que executa a ferramenta), "
    + "depois tente de novo."

func writeStdout(_ object: [String: Any]) {
    let data = (try? JSONSerialization.data(withJSONObject: object, options: []))
        ?? Data("{\"ok\":false,\"error\":\"falha ao serializar JSON\"}".utf8)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
}

func succeed(_ object: [String: Any]) -> Never {
    var payload = object
    payload["ok"] = true
    writeStdout(payload)
    exit(0)
}

func fail(_ message: String) -> Never {
    writeStdout(["ok": false, "error": message])
    // stderr mirrors the error so the Rust side has a readable exit message.
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

func requireTrusted() {
    if !AXIsProcessTrusted() {
        fail(accessibilityHelp)
    }
}

func eventSource() -> CGEventSource {
    guard let source = CGEventSource(stateID: .hidSystemState) else {
        fail("não foi possível criar a fonte de eventos do sistema")
    }
    return source
}

// MARK: - Key map (US/ANSI layout)

let keyCodes: [String: CGKeyCode] = [
    "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5,
    "z": 6, "x": 7, "c": 8, "v": 9, "b": 11,
    "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
    "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23,
    "equal": 24, "=": 24,
    "9": 25, "7": 26,
    "minus": 27, "-": 27,
    "8": 28, "0": 29,
    "bracketright": 30, "]": 30,
    "o": 31, "u": 32,
    "bracketleft": 33, "[": 33,
    "i": 34, "p": 35,
    "return": 36, "enter": 36,
    "l": 37, "j": 38,
    "quote": 39, "'": 39,
    "k": 40,
    "semicolon": 41, ";": 41,
    "backslash": 42, "\\": 42,
    "comma": 43, ",": 43,
    "slash": 44, "/": 44,
    "n": 45, "m": 46,
    "period": 47, ".": 47,
    "tab": 48,
    "space": 49, "spacebar": 49,
    "grave": 50, "`": 50,
    "backspace": 51, "delete": 51,
    "escape": 53, "esc": 53,
    "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97,
    "f7": 98, "f8": 100, "f9": 101, "f10": 109, "f11": 103, "f12": 111,
    "f13": 105, "f14": 107, "f15": 113, "f16": 106, "f17": 64,
    "f18": 79, "f19": 80,
    "help": 114,
    "home": 115,
    "pageup": 116,
    "forwarddelete": 117, "fwddelete": 117, "del": 117,
    "end": 119,
    "pagedown": 121,
    "left": 123, "arrowleft": 123,
    "right": 124, "arrowright": 124,
    "down": 125, "arrowdown": 125,
    "up": 126, "arrowup": 126,
    "plus": 24,
]

func modifierFlag(_ token: String) -> CGEventFlags? {
    switch token {
    case "cmd", "command", "meta", "super":
        return .maskCommand
    case "shift":
        return .maskShift
    case "alt", "option", "opt":
        return .maskAlternate
    case "ctrl", "control":
        return .maskControl
    case "fn":
        return .maskSecondaryFn
    case "capslock", "caps":
        return .maskAlphaShift
    default:
        return nil
    }
}

func postKey(_ code: CGKeyCode, flags: CGEventFlags) {
    let source = eventSource()
    guard
        let down = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: true),
        let up = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: false)
    else {
        fail("não foi possível criar o evento de teclado para o código \(code)")
    }
    down.flags = flags
    up.flags = flags
    down.post(tap: .cghidEventTap)
    usleep(8_000)
    up.post(tap: .cghidEventTap)
}

func parseKeyCombo(_ combo: String) -> (CGKeyCode, CGEventFlags) {
    let rawTokens = combo
        .split(separator: "+", omittingEmptySubsequences: false)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() }

    var flags: CGEventFlags = []
    var keyCode: CGKeyCode?

    for token in rawTokens {
        if token.isEmpty {
            continue
        }
        if let flag = modifierFlag(token) {
            flags.insert(flag)
            continue
        }
        guard let code = keyCodes[token] else {
            fail("tecla desconhecida em `\(combo)`: `\(token)`")
        }
        if keyCode != nil {
            fail("a combinação `\(combo)` tem mais de uma tecla final")
        }
        keyCode = code
    }

    guard let code = keyCode else {
        fail("a combinação `\(combo)` não tem uma tecla final (ex.: cmd+shift+4)")
    }
    return (code, flags)
}

// MARK: - Typing

func typeText(_ text: String) {
    let source = eventSource()

    for character in text {
        if character == "\n" || character == "\r" {
            postKey(36, flags: [])
            continue
        }
        if character == "\t" {
            postKey(48, flags: [])
            continue
        }

        var units = Array(String(character).utf16)
        guard
            let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
            let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false)
        else {
            fail("não foi possível criar o evento de teclado")
        }
        down.keyboardSetUnicodeString(stringLength: units.count, unicodeString: &units)
        up.keyboardSetUnicodeString(stringLength: units.count, unicodeString: &units)
        down.post(tap: .cghidEventTap)
        usleep(1_000)
        up.post(tap: .cghidEventTap)
        usleep(3_000)
    }
}

// MARK: - Mouse

func mouseEvent(
    _ type: CGEventType,
    at point: CGPoint,
    button: CGMouseButton,
    clickState: Int64 = 1
) -> CGEvent {
    let source = eventSource()
    guard
        let event = CGEvent(
            mouseEventSource: source,
            mouseType: type,
            mouseCursorPosition: point,
            mouseButton: button
        )
    else {
        fail("não foi possível criar o evento de mouse")
    }
    event.setIntegerValueField(.mouseEventClickState, value: clickState)
    return event
}

func click(x: Double, y: Double, button: String, count: Int) {
    let point = CGPoint(x: x, y: y)
    var downType: CGEventType = .leftMouseDown
    var upType: CGEventType = .leftMouseUp
    var cgButton: CGMouseButton = .left

    switch button {
    case "left":
        downType = .leftMouseDown
        upType = .leftMouseUp
        cgButton = .left
    case "right":
        downType = .rightMouseDown
        upType = .rightMouseUp
        cgButton = .right
    case "center", "middle":
        downType = .otherMouseDown
        upType = .otherMouseUp
        cgButton = .center
    default:
        fail("botão desconhecido: `\(button)` (use left, right ou center)")
    }

    let clicks = max(1, count)
    for index in 1...clicks {
        let state = Int64(index)
        let down = mouseEvent(downType, at: point, button: cgButton, clickState: state)
        down.post(tap: .cghidEventTap)
        usleep(12_000)
        let up = mouseEvent(upType, at: point, button: cgButton, clickState: state)
        up.post(tap: .cghidEventTap)
        usleep(12_000)
    }
}

func moveMouse(x: Double, y: Double) {
    let point = CGPoint(x: x, y: y)
    let event = mouseEvent(.mouseMoved, at: point, button: .left)
    event.post(tap: .cghidEventTap)
}

func scrollWheel(dx: Int32, dy: Int32) {
    let source = eventSource()
    guard
        let event = CGEvent(
            scrollWheelEvent2Source: source,
            units: .pixel,
            wheelCount: 2,
            wheel1: dy,
            wheel2: dx,
            wheel3: 0
        )
    else {
        fail("não foi possível criar o evento de rolagem")
    }
    event.post(tap: .cghidEventTap)
}

func dragMouse(from: CGPoint, to: CGPoint) {
    let steps = 24
    let down = mouseEvent(.leftMouseDown, at: from, button: .left)
    down.post(tap: .cghidEventTap)
    usleep(60_000)

    for step in 1...steps {
        let progress = Double(step) / Double(steps)
        let point = CGPoint(
            x: from.x + (to.x - from.x) * progress,
            y: from.y + (to.y - from.y) * progress
        )
        let moved = mouseEvent(.leftMouseDragged, at: point, button: .left)
        moved.post(tap: .cghidEventTap)
        usleep(8_000)
    }

    usleep(40_000)
    let up = mouseEvent(.leftMouseUp, at: to, button: .left)
    up.post(tap: .cghidEventTap)
}

// MARK: - Argument parsing

func parseDouble(_ raw: String, label: String) -> Double {
    guard let value = Double(raw), value.isFinite else {
        fail("\(label) inválido: `\(raw)`")
    }
    return value
}

func parseCoordinate(_ raw: String, label: String) -> Double {
    let value = parseDouble(raw, label: label)
    guard abs(value) <= 100_000 else {
        fail("\(label) fora do intervalo razoável: `\(raw)`")
    }
    return value
}

// MARK: - Entry point

let arguments = Array(CommandLine.arguments.dropFirst())

guard let command = arguments.first?.lowercased() else {
    fail("uso: input <trusted|type|key|hotkey|click|move|scroll|drag> ...")
}

let rest = Array(arguments.dropFirst())

switch command {
case "trusted":
    succeed(["trusted": AXIsProcessTrusted()])

case "type":
    requireTrusted()
    let text = rest.joined(separator: " ")
    guard !text.isEmpty else {
        fail("informe o texto a digitar: input type <texto>")
    }
    typeText(text)
    succeed(["action": "type", "chars": text.count])

case "key", "hotkey":
    requireTrusted()
    guard let combo = rest.first, !combo.isEmpty else {
        fail("informe o atalho: input key <combinação> (ex.: cmd+shift+4)")
    }
    let (code, flags) = parseKeyCombo(combo)
    postKey(code, flags: flags)
    var payload: [String: Any] = [
        "action": "key",
        "combo": combo,
        "key_code": Int(code),
    ]
    if command == "hotkey" {
        payload["note"] =
            "hotkey é um alias de key; não registra atalhos globais persistentes "
            + "(nenhum run loop é iniciado)"
    }
    succeed(payload)

case "click":
    requireTrusted()
    guard rest.count >= 2 else {
        fail("uso: input click <x> <y> [left|right|center] [count]")
    }
    let x = parseCoordinate(rest[0], label: "x")
    let y = parseCoordinate(rest[1], label: "y")
    let button = rest.count > 2 ? rest[2].lowercased() : "left"
    var count = 1
    if rest.count > 3 {
        guard let parsed = Int(rest[3]), (1...3).contains(parsed) else {
            fail("count inválido: `\(rest[3])` (use 1, 2 ou 3)")
        }
        count = parsed
    }
    click(x: x, y: y, button: button, count: count)
    succeed([
        "action": "click",
        "x": x,
        "y": y,
        "button": button,
        "count": count,
    ])

case "move":
    requireTrusted()
    guard rest.count >= 2 else {
        fail("uso: input move <x> <y>")
    }
    let x = parseCoordinate(rest[0], label: "x")
    let y = parseCoordinate(rest[1], label: "y")
    moveMouse(x: x, y: y)
    succeed(["action": "move", "x": x, "y": y])

case "scroll":
    requireTrusted()
    guard rest.count >= 2 else {
        fail("uso: input scroll <dx> <dy>")
    }
    let dxValue = parseDouble(rest[0], label: "dx")
    let dyValue = parseDouble(rest[1], label: "dy")
    guard abs(dxValue) <= 100_000, abs(dyValue) <= 100_000 else {
        fail("deslocamento de rolagem fora do intervalo razoável")
    }
    scrollWheel(dx: Int32(dxValue.rounded()), dy: Int32(dyValue.rounded()))
    succeed(["action": "scroll", "dx": Int(dxValue), "dy": Int(dyValue)])

case "drag":
    requireTrusted()
    guard rest.count >= 4 else {
        fail("uso: input drag <x1> <y1> <x2> <y2>")
    }
    let fromX = parseCoordinate(rest[0], label: "x1")
    let fromY = parseCoordinate(rest[1], label: "y1")
    let toX = parseCoordinate(rest[2], label: "x2")
    let toY = parseCoordinate(rest[3], label: "y2")
    dragMouse(from: CGPoint(x: fromX, y: fromY), to: CGPoint(x: toX, y: toY))
    succeed([
        "action": "drag",
        "from": ["x": fromX, "y": fromY],
        "to": ["x": toX, "y": toY],
    ])

default:
    fail("subcomando desconhecido: `\(command)` (use trusted, type, key, hotkey, click, move, scroll, drag)")
}
