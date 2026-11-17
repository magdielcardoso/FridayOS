// FridayOS native event sources.
//
// A long-lived helper: it reads one command per line from stdin and writes one
// JSON object per line to stdout (always flushed). It never touches the network.
//
// Commands (stdin):
//   watch-files <comma,separated,paths>   FSEvents watch (recursive where applicable)
//   watch-apps                           NSWorkspace activate / launch / terminate
//   watch-clipboard                      NSPasteboard.changeCount poll (250 ms)
//   watch-mail                           lightweight new-mail count, if available
//   watch-imessage                       chat.db file change (no content is read)
//   stop <files|apps|clipboard|mail|imessage|all>
//   status
//   quit
//
// Events (stdout):
//   {"event":"change","path":"...","flag":"created|modified|deleted|renamed"}
//   {"event":"dropped","count":N}
//   {"event":"app","name":"...","bundle":"..."}
//   {"event":"clipboard","chars":N,"preview":"...first 120 chars..."}
//   {"event":"mail","count":N}
//   {"event":"imessage"}
//   {"event":"watching","kind":"..."}   command acknowledgement
//   {"event":"status",...}
//   {"event":"error","message":"..."}
//
// Build: xcrun swiftc -O -o <cache>/watcher_events watcher_events.swift
//
// macOS caveats:
// - FSEvents deliveries require the main run loop. Commands are therefore read
//   on a background thread and executed on the main queue; the main thread just
//   spins RunLoop.main.
// - Mail store access is usually blocked by TCC for a standalone binary. When
//   the local Mail store cannot be read we emit one `error` and keep running.
//   No mail, message or clipboard *content* is ever transmitted (counts only,
//   plus a redacted clipboard preview).
// - Every framework use is best-effort: a missing/unavailable source degrades
//   to an error event rather than crashing the helper.

import Foundation
import AppKit
import CoreServices

// MARK: - Output

private let outputLock = NSLock()

func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: []),
          let line = String(data: data, encoding: .utf8) else { return }
    outputLock.lock()
    print(line)
    fflush(stdout)
    outputLock.unlock()
}

func emitError(_ message: String) {
    emit(["event": "error", "message": message])
}

/// Some sources are expected to be unavailable; report each only once.
private var reportedErrors = Set<String>()
func emitErrorOnce(_ key: String, _ message: String) {
    guard reportedErrors.insert(key).inserted else { return }
    emitError(message)
}

// MARK: - Change coordinator (200 ms debounce, max 40 events/sec)

private struct ChangeRecord {
    let label: String // "files" | "imessage"
    let path: String
    let flag: String
}

private final class ChangeCoordinator {
    private let lock = NSLock()
    private var pending: [String: ChangeRecord] = [:]
    private var debounceTimer: Timer?
    private var emitTimes: [Date] = []

    private let debounce: TimeInterval = 0.2
    private let maxPerSecond = 40

    /// Called on the main run loop from FSEvents callbacks.
    func enqueue(_ record: ChangeRecord) {
        lock.lock()
        let key = "\(record.label)\u{0}\(record.flag)\u{0}\(record.path)"
        pending[key] = record
        if debounceTimer == nil {
            debounceTimer = Timer.scheduledTimer(withTimeInterval: debounce, repeats: false) { [weak self] _ in
                self?.flush()
            }
        }
        lock.unlock()
    }

    private func flush() {
        lock.lock()
        debounceTimer?.invalidate()
        debounceTimer = nil
        let batch = Array(pending.values)
        pending.removeAll(keepingCapacity: true)
        lock.unlock()

        guard !batch.isEmpty else { return }
        var dropped = 0
        for record in batch {
            if allow() {
                if record.label == "imessage" {
                    // Deliberately content-free: a debounced presence signal.
                    emit(["event": "imessage"])
                } else {
                    emit(["event": "change", "path": record.path, "flag": record.flag])
                }
            } else {
                dropped += 1
            }
        }
        if dropped > 0 {
            emit(["event": "dropped", "count": dropped])
        }
    }

    /// Sliding one-second window: at most `maxPerSecond` emissions.
    private func allow() -> Bool {
        let now = Date()
        emitTimes = emitTimes.filter { now.timeIntervalSince($0) < 1.0 }
        if emitTimes.count >= maxPerSecond { return false }
        emitTimes.append(now)
        return true
    }
}

private let coordinator = ChangeCoordinator()

// MARK: - FSEvents bridge

private struct FSEventRecord {
    let path: String
    let flag: String
}

/// Owns one stream's delivery closure. The C callback cannot capture, so the
/// handler is kept in a global registry and referenced by an integer token.
/// `active` lets `stop()` invalidate in-flight callbacks: a queued FSEvents
/// block may still run after the stream is torn down, and must not emit then.
private final class FSEventHandler {
    let sink: ([FSEventRecord]) -> Void
    private let lock = NSLock()
    private var active = true

    init(_ sink: @escaping ([FSEventRecord]) -> Void) { self.sink = sink }

    func deliver(_ records: [FSEventRecord]) {
        lock.lock()
        let isActive = active
        lock.unlock()
        guard isActive else { return }
        sink(records)
    }

    func close() {
        lock.lock()
        active = false
        lock.unlock()
    }
}

// A token -> handler registry keeps handlers alive for as long as the stream
// may call back. The lookup returns a strong reference, so even if `stop()`
// removes the entry concurrently a callback in flight cannot use freed memory.
private let handlerLock = NSLock()
private var handlerRegistry: [UInt: FSEventHandler] = [:]
private var handlerNextToken: UInt = 1

private func registerHandler(_ handler: FSEventHandler) -> UInt {
    handlerLock.lock()
    let token = handlerNextToken
    handlerNextToken &+= 1
    handlerRegistry[token] = handler
    handlerLock.unlock()
    return token
}

private func lookupHandler(_ token: UInt) -> FSEventHandler? {
    handlerLock.lock()
    let handler = handlerRegistry[token]
    handlerLock.unlock()
    return handler
}

private func unregisterHandler(_ token: UInt) {
    handlerLock.lock()
    let handler = handlerRegistry.removeValue(forKey: token)
    handlerLock.unlock()
    handler?.close()
}

private let fsEventCallback: FSEventStreamCallback = { _, info, count, eventPaths, eventFlags, _ in
    guard let info = info else { return }
    let token = UInt(bitPattern: info)
    guard let handler = lookupHandler(token) else { return }
    let cfPaths = Unmanaged<CFArray>.fromOpaque(eventPaths).takeUnretainedValue()
    let paths = cfPaths as? [String] ?? []

    var records: [FSEventRecord] = []
    records.reserveCapacity(count)
    for i in 0..<count {
        let path = i < paths.count ? paths[i] : ""
        let flags = eventFlags[i]
        let flag: String
        if flags & FSEventStreamEventFlags(kFSEventStreamEventFlagItemRenamed) != 0 {
            flag = "renamed"
        } else if flags & FSEventStreamEventFlags(kFSEventStreamEventFlagItemCreated) != 0 {
            flag = "created"
        } else if flags & FSEventStreamEventFlags(kFSEventStreamEventFlagItemRemoved) != 0 {
            flag = "deleted"
        } else {
            flag = "modified"
        }
        records.append(FSEventRecord(path: path, flag: flag))
    }
    handler.deliver(records)
}

/// Owns one FSEvents stream. All methods are called on the main thread.
private final class FSEventsWatch {
    private var stream: FSEventStreamRef?
    private var token: UInt?

    init?(paths: [String], latency: TimeInterval, sink: @escaping ([FSEventRecord]) -> Void) {
        guard !paths.isEmpty else { return nil }

        let token = registerHandler(FSEventHandler(sink))
        guard let ptr = UnsafeMutableRawPointer(bitPattern: token) else {
            unregisterHandler(token)
            return nil
        }
        var context = FSEventStreamContext(version: 0, info: ptr, retain: nil, release: nil, copyDescription: nil)
        let flags = FSEventStreamCreateFlags(kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer)

        guard let created = FSEventStreamCreate(
            nil,
            fsEventCallback,
            &context,
            paths as CFArray,
            FSEventStreamEventId(kFSEventStreamEventIdSinceNow),
            latency,
            flags
        ) else {
            unregisterHandler(token)
            return nil
        }

        self.stream = created
        self.token = token
        // Deliver callbacks on the main queue (avoids the deprecated run-loop
        // scheduling API while keeping all watcher state on one thread).
        FSEventStreamSetDispatchQueue(created, DispatchQueue.main)
        if !FSEventStreamStart(created) {
            stop()
        }
    }

    func stop() {
        guard let stream = stream else { return }
        // Invalidate the sink first: any callback already queued on the main
        // queue will see `active == false` and emit nothing.
        if let token = token {
            unregisterHandler(token)
            self.token = nil
        }
        FSEventStreamStop(stream)
        FSEventStreamSetDispatchQueue(stream, nil)
        FSEventStreamInvalidate(stream)
        FSEventStreamRelease(stream)
        self.stream = nil
    }

    deinit { stop() }
}

// MARK: - Watchers

private final class AppWatch {
    private var tokens: [NSObjectProtocol] = []

    init() {
        let center = NSWorkspace.shared.notificationCenter
        let names = [
            NSWorkspace.didActivateApplicationNotification,
            NSWorkspace.didLaunchApplicationNotification,
            NSWorkspace.didTerminateApplicationNotification,
        ]
        for name in names {
            tokens.append(center.addObserver(forName: name, object: nil, queue: .main) { note in
                guard let app = note.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication else { return }
                emit([
                    "event": "app",
                    "name": app.localizedName ?? "",
                    "bundle": app.bundleIdentifier ?? "",
                ])
            })
        }
    }

    func stop() {
        let center = NSWorkspace.shared.notificationCenter
        tokens.forEach { center.removeObserver($0) }
        tokens.removeAll()
    }

    deinit { stop() }
}

private final class ClipboardWatch {
    private var timer: Timer?
    private var lastCount: Int = -1

    init() {
        lastCount = NSPasteboard.general.changeCount
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            self?.tick()
        }
    }

    private func tick() {
        let pb = NSPasteboard.general
        let count = pb.changeCount
        guard count != lastCount else { return }
        lastCount = count
        let text = pb.string(forType: .string) ?? ""
        emit([
            "event": "clipboard",
            "chars": text.count,
            "preview": Self.preview(text),
        ])
    }

    /// Never transmit full content, and redact anything that looks like a secret.
    private static func preview(_ text: String) -> String {
        let lower = text.lowercased()
        let markers = [
            "password", "senha", "secret", "token", "api_key", "apikey",
            "api-key", "private key", "begin rsa", "begin private", "credential",
        ]
        if markers.contains(where: { lower.contains($0) }) {
            return "[redacted]"
        }
        let flat = text.replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression)
        return String(flat.prefix(120))
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    deinit { stop() }
}

private final class MailWatch {
    private var fs: FSEventsWatch?
    private var dir: String?

    init() {
        let fm = FileManager.default
        let home = NSHomeDirectory()
        let candidates = [
            home + "/Library/Mail",
            home + "/Library/Containers/com.apple.mail/Data/Library/Mail",
        ]
        guard let found = candidates.first(where: { fm.fileExists(atPath: $0) }) else {
            emitErrorOnce("mail-unavailable", "watch-mail: Mail store indisponível; monitoramento desativado")
            return
        }
        dir = found
        emit(["event": "mail", "count": Self.countMessages(in: found)])
        fs = FSEventsWatch(paths: [found], latency: 1.0) { [weak self] _ in
            guard let self = self, let dir = self.dir else { return }
            emit(["event": "mail", "count": Self.countMessages(in: dir)])
        }
        if fs == nil {
            emitErrorOnce("mail-watch-failed", "watch-mail: não foi possível observar a pasta de e-mail")
        }
    }

    /// Counts `.emlx` files. Returns 0 when TCC blocks enumeration (graceful).
    private static func countMessages(in dir: String) -> Int {
        guard let subpaths = try? FileManager.default.subpathsOfDirectory(atPath: dir) else { return 0 }
        return subpaths.reduce(0) { $0 + ($1.hasSuffix(".emlx") ? 1 : 0) }
    }

    func stop() {
        fs?.stop()
        fs = nil
        dir = nil
    }

    deinit { stop() }
}

private final class Watchers {
    var files: FSEventsWatch?
    var apps: AppWatch?
    var clipboard: ClipboardWatch?
    var mail: MailWatch?
    var imessage: FSEventsWatch?

    func stop(_ kind: String) {
        switch kind {
        case "files":   files?.stop(); files = nil
        case "apps":    apps?.stop(); apps = nil
        case "clipboard": clipboard?.stop(); clipboard = nil
        case "mail":    mail?.stop(); mail = nil
        case "imessage": imessage?.stop(); imessage = nil
        case "all":     stopAll()
        default:        emitError("stop: unknown kind \(kind)")
        }
    }

    func stopAll() {
        stop("files")
        stop("apps")
        stop("clipboard")
        stop("mail")
        stop("imessage")
    }
}

private let watchers = Watchers()

// MARK: - Commands (executed on the main queue)

private func startFiles(_ rest: String) {
    let paths = rest.split(separator: ",", omittingEmptySubsequences: true)
        .map(String.init)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        .filter { !$0.isEmpty }
        .map { ($0 as NSString).expandingTildeInPath }
        .map { URL(fileURLWithPath: $0).standardizedFileURL.path }

    guard !paths.isEmpty else {
        emitError("watch-files: missing paths")
        return
    }

    watchers.files?.stop()
    watchers.files = FSEventsWatch(paths: paths, latency: 0.2) { records in
        for record in records {
            coordinator.enqueue(ChangeRecord(label: "files", path: record.path, flag: record.flag))
        }
    }
    if watchers.files == nil {
        emitError("watch-files: could not create FSEvents stream")
    } else {
        emit(["event": "watching", "kind": "files"])
    }
}

private func startIMessage() {
    guard watchers.imessage == nil else { return }
    let db = NSHomeDirectory() + "/Library/Messages/chat.db"
    let dir = (db as NSString).deletingLastPathComponent
    // Watch the directory so journal/WAL writes are seen too. Only chat.db
    // paths are relevant; message content is never opened.
    watchers.imessage = FSEventsWatch(paths: [dir], latency: 0.2) { records in
        let relevant = records.contains { $0.path.hasSuffix("/chat.db") || $0.path.hasSuffix("chat.db") }
        if relevant {
            coordinator.enqueue(ChangeRecord(label: "imessage", path: db, flag: "modified"))
        }
    }
    if watchers.imessage == nil {
        emitErrorOnce("imessage-unavailable", "watch-imessage: chat.db indisponível")
    } else {
        emit(["event": "watching", "kind": "imessage"])
    }
}

private let mainRunLoop = RunLoop.main.getCFRunLoop()

private func execute(_ command: String, _ rest: String) {
    switch command {
    case "status":
        emit([
            "event": "status",
            "available": true,
            "files": watchers.files != nil,
            "apps": watchers.apps != nil,
            "clipboard": watchers.clipboard != nil,
            "mail": watchers.mail != nil,
            "imessage": watchers.imessage != nil,
        ])
    case "watch-files":
        startFiles(rest)
    case "watch-apps":
        if watchers.apps == nil { watchers.apps = AppWatch() }
        emit(["event": "watching", "kind": "apps"])
    case "watch-clipboard":
        if watchers.clipboard == nil { watchers.clipboard = ClipboardWatch() }
        emit(["event": "watching", "kind": "clipboard"])
    case "watch-mail":
        if watchers.mail == nil { watchers.mail = MailWatch() }
        emit(["event": "watching", "kind": "mail"])
    case "watch-imessage":
        startIMessage()
    case "stop":
        watchers.stop(rest)
    case "quit":
        watchers.stopAll()
        emit(["event": "quit"])
        CFRunLoopStop(mainRunLoop)
    default:
        emitError("unknown command: \(command)")
    }
}

// MARK: - Command loop

private func readLoop() {
    while let line = readLine(strippingNewline: true) {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { continue }
        let parts = trimmed.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true).map(String.init)
        guard let command = parts.first else { continue }
        let rest = parts.count > 1 ? parts[1].trimmingCharacters(in: .whitespaces) : ""
        DispatchQueue.main.async { execute(command, rest) }
    }
    // stdin closed: tear down so the process exits cleanly.
    DispatchQueue.main.async {
        watchers.stopAll()
        CFRunLoopStop(mainRunLoop)
    }
}

let argv = Array(CommandLine.arguments.dropFirst())
if !argv.isEmpty {
    // One-shot mode for direct CLI use: e.g. `watcher_events status`.
    let parts = argv.joined(separator: " ").split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true).map(String.init)
    execute(parts.first ?? "", parts.count > 1 ? parts[1] : "")
} else {
    let reader = Thread { readLoop() }
    reader.stackSize = 256 * 1024
    reader.start()
    // A repeating timer guarantees the main run loop has a source even when the
    // only active watcher is an FSEvents stream, so the helper cannot exit early.
    let keepAlive = Timer.scheduledTimer(withTimeInterval: 3600.0, repeats: true) { _ in }
    withExtendedLifetime(keepAlive) {
        RunLoop.main.run()
    }
}
