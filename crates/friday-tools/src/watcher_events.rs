//! Native macOS event sources, exposed as a thin wrapper over a long-lived
//! Swift helper (see `swift/watcher_events.swift`).
//!
//! The helper reads commands from stdin and emits one JSON object per line on
//! stdout. We keep a single instance in a global mutex that holds only the
//! child process and its stdin; stdout is drained by a dedicated background
//! thread into an mpsc channel. [`next_event`] therefore never holds a mutex
//! across a blocking read - it briefly parks the channel receiver, does a
//! `recv_timeout`, and parks it back.
//!
//! The helper is compiled lazily and cached under
//! `~/Library/Caches/FridayOS/bin` by [`crate::swift::helper`].
//!
//! NOTE: this module is intentionally standalone. Wire it into the crate by
//! adding `pub mod watcher_events;` to `src/lib.rs` from the integrator.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Proc {
    child: Child,
    stdin: ChildStdin,
}

/// Guards the helper process and its stdin only. Never used for stdout reads.
static PROC: Mutex<Option<Proc>> = Mutex::new(None);
/// Parking spot for the event receiver. The mutex is held only to take/put the
/// receiver back - never while `recv_timeout` is blocking.
static EVENT_RX: Mutex<Option<Receiver<Value>>> = Mutex::new(None);
/// Events consumed out of order by [`status`] (e.g. a change arriving while it
/// waits for the status reply) are queued here and returned first by the next
/// [`next_event`] call, so no event is lost.
static PENDING: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());

/// Bounded memory of recently observed native events. This is populated by the
/// stdout reader thread (see `ensure_proc`), so it reflects activity even when
/// nobody drains the event channel. It never grows past [`RING_CAP`].
static NATIVE_RING: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());
const RING_CAP: usize = 50;
/// Event kinds forwarded to live reasoning. Protocol chatter (`status`,
/// `error`, `watching`, `dropped`, `quit`) is deliberately excluded.
const NATIVE_KINDS: [&str; 5] = ["change", "app", "clipboard", "imessage", "mail"];

fn is_native_event(value: &Value) -> bool {
    value
        .get("event")
        .and_then(Value::as_str)
        .map_or(false, |event| NATIVE_KINDS.contains(&event))
}

/// Records an interesting event in the bounded ring. Best-effort: a poisoned
/// lock simply drops the observation instead of panicking.
fn record_event(value: &Value) {
    if !is_native_event(value) {
        return;
    }
    if let Ok(mut ring) = NATIVE_RING.lock() {
        if ring.len() >= RING_CAP {
            ring.pop_front();
        }
        ring.push_back(value.clone());
    }
}

fn proc_lock() -> Result<MutexGuard<'static, Option<Proc>>, String> {
    PROC.lock().map_err(|e| format!("watcher_events lock envenenado: {e}"))
}

fn ensure_proc<'a>(guard: &'a mut MutexGuard<'static, Option<Proc>>) -> Result<&'a mut Proc, String> {
    if guard.is_none() {
        let binary = crate::swift::helper("watcher_events").map_err(|e| e.to_string())?;
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Never leave an undrained stderr pipe that could fill and block.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("watcher_events helper: {e}"))?;
        let stdin = child.stdin.take().ok_or("watcher_events: stdin indisponível")?;
        let stdout = child.stdout.take().ok_or("watcher_events: stdout indisponível")?;

        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Ignore blank/non-JSON chatter so the protocol stays robust.
                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                // Observe every event as it arrives, independently of who
                // drains the channel. Keeps `native_event_summary()` live.
                record_event(&value);
                if tx.send(value).is_err() {
                    break;
                }
            }
        });

        {
            let mut rx_guard = EVENT_RX
                .lock()
                .map_err(|e| format!("watcher_events event lock envenenado: {e}"))?;
            *rx_guard = Some(rx);
        }

        **guard = Some(Proc { child, stdin });
    }
    Ok(guard.as_mut().expect("helper acabado de iniciar"))
}

/// Lazily spawns the helper if needed, without keeping the lock afterwards.
fn ensure_started() -> Result<(), String> {
    let mut guard = proc_lock()?;
    let _ = ensure_proc(&mut guard)?;
    Ok(())
}

fn send_command(command: &str) -> Result<(), String> {
    let mut guard = proc_lock()?;
    let proc = ensure_proc(&mut guard)?;
    writeln!(proc.stdin, "{}", command)
        .and_then(|_| proc.stdin.flush())
        .map_err(|e| format!("watcher_events stdin: {e}"))
}

/// Helper/stream status as reported by the Swift side.
pub fn status() -> Result<Value, String> {
    send_command("status")?;
    let deadline = Instant::now() + Duration::from_millis(2_000);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err("watcher_events: timeout aguardando status".to_string());
        }
        let remaining = (deadline - now).as_millis().min(u32::MAX as u128) as u32;
        match next_event(remaining)? {
            Some(value) => {
                let event = value.get("event").and_then(Value::as_str).unwrap_or("");
                if event == "status" {
                    return Ok(value);
                }
                if event == "error" {
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("erro desconhecido do helper");
                    return Err(message.to_string());
                }
                // Not the reply we are waiting for: keep it for later.
                let mut pending = PENDING
                    .lock()
                    .map_err(|e| format!("watcher_events pending lock envenenado: {e}"))?;
                pending.push_back(value);
            }
            None => return Err("watcher_events: helper não respondeu ao status".to_string()),
        }
    }
}

/// Starts an FSEvents watch over `paths`. Paths containing commas or newlines
/// are rejected because the line protocol uses those as separators.
pub fn start_files(paths: &[&Path]) -> Result<(), String> {
    let rendered = paths
        .iter()
        .map(|path| {
            let text = path.to_string_lossy().into_owned();
            if text.contains(',') || text.contains('\n') || text.contains('\r') {
                return Err(format!("caminho inválido para watch-files: {text}"));
            }
            Ok(text)
        })
        .collect::<Result<Vec<_>, String>>()?;
    if rendered.is_empty() {
        return Err("watch-files: nenhum caminho fornecido".to_string());
    }
    send_command(&format!("watch-files {}", rendered.join(",")))
}

/// Watches application activate / launch / terminate notifications.
pub fn start_apps() -> Result<(), String> {
    send_command("watch-apps")
}

/// Polls the pasteboard change count every 250 ms (content is never stored).
pub fn start_clipboard() -> Result<(), String> {
    send_command("watch-clipboard")
}

/// Watches new mail (best effort; a TCC-blocked source emits one error event).
pub fn start_mail() -> Result<(), String> {
    send_command("watch-mail")
}

/// Watches `~/Library/Messages/chat.db` for changes without reading content.
pub fn start_imessage() -> Result<(), String> {
    send_command("watch-imessage")
}

/// Starts the full set of native sources used for live context: application
/// activations, clipboard changes, iMessage database changes and an FSEvents
/// watch over the local knowledge folder.
///
/// Every source is attempted even if an earlier one fails; the first (or all)
/// failures are returned joined. The helper itself is started lazily by
/// [`send_command`]; unavailable OS sources only emit an `error` event and do
/// not make this call fail.
pub fn start_native_sources() -> Result<(), String> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let knowledge = PathBuf::from(home)
        .join("Library/Application Support/FridayOS/knowledge");
    // An FSEvents stream on a missing directory delivers nothing, so make sure
    // the folder exists before watching it.
    let _ = std::fs::create_dir_all(&knowledge);

    let mut errors: Vec<String> = Vec::new();
    if let Err(error) = start_apps() {
        errors.push(error);
    }
    if let Err(error) = start_clipboard() {
        errors.push(error);
    }
    if let Err(error) = start_imessage() {
        errors.push(error);
    }
    if let Err(error) = start_files(&[knowledge.as_path()]) {
        errors.push(error);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Drains up to `limit` native events from [`next_event`] with a zero timeout.
///
/// The whole call is capped at ~20 ms: it never waits for events to arrive and
/// stops early if the channel is momentarily empty. `status`/`error` and other
/// protocol chatter are skipped; only `change`, `app`, `clipboard`, `imessage`
/// and `mail` are returned. Events are *not* re-recorded here — the stdout
/// reader already feeds the bounded summary ring.
pub fn take_native_events(limit: usize) -> Vec<Value> {
    let mut events = Vec::new();
    if limit == 0 {
        return events;
    }
    // Absorb the one-off helper build/spawn cost before starting the clock.
    if ensure_started().is_err() {
        return events;
    }
    let deadline = Instant::now() + Duration::from_millis(20);
    for _ in 0..limit {
        if Instant::now() >= deadline {
            break;
        }
        match next_event(0) {
            Ok(Some(value)) => {
                if is_native_event(&value) {
                    events.push(value);
                }
            }
            // No event ready (or helper disconnected): don't spin waiting.
            Ok(None) | Err(_) => break,
        }
    }
    events
}

/// Compact, bounded summary of the most recent native activity.
///
/// Reads the 50-event ring maintained by the stdout reader. `count` is the
/// number of observations currently retained; the other fields describe the
/// newest matching event of each kind. Never blocks on the helper.
pub fn native_event_summary() -> Value {
    let mut count = 0usize;
    let mut last_app = String::new();
    let mut last_clipboard_chars = Value::Null;
    let mut last_paths: Vec<String> = Vec::new();

    if let Ok(ring) = NATIVE_RING.lock() {
        count = ring.len();
        for event in ring.iter().rev() {
            match event.get("event").and_then(Value::as_str).unwrap_or("") {
                "app" if last_app.is_empty() => {
                    last_app = event
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                "clipboard" if last_clipboard_chars.is_null() => {
                    if let Some(chars) = event.get("chars").and_then(Value::as_i64) {
                        last_clipboard_chars = json!(chars);
                    }
                }
                "change" | "imessage" => {
                    if let Some(path) = event.get("path").and_then(Value::as_str) {
                        if !path.is_empty()
                            && last_paths.len() < 5
                            && !last_paths.iter().any(|seen| seen == path)
                        {
                            last_paths.push(path.to_string());
                        }
                    }
                }
                _ => {}
            }
            if !last_app.is_empty() && !last_clipboard_chars.is_null() && last_paths.len() >= 5 {
                break;
            }
        }
    }

    json!({
        "count": count,
        "last_app": last_app,
        "last_clipboard_chars": last_clipboard_chars,
        "last_paths": last_paths,
    })
}

/// Stops one watcher. `kind` is one of `files`, `apps`, `clipboard`, `mail`,
/// `imessage` or `all`. Stopping when the helper is not running is a no-op.
pub fn stop(kind: &str) -> Result<(), String> {
    const KINDS: &[&str] = &["files", "apps", "clipboard", "mail", "imessage", "all"];
    if !KINDS.contains(&kind) {
        return Err(format!("watcher_events: kind desconhecido: {kind}"));
    }
    let mut guard = proc_lock()?;
    match guard.as_mut() {
        Some(proc) => writeln!(proc.stdin, "stop {}", kind)
            .and_then(|_| proc.stdin.flush())
            .map_err(|e| format!("watcher_events stdin: {e}")),
        None => Ok(()),
    }
}

/// Returns the next event, waiting at most `timeout_ms` milliseconds.
///
/// `Ok(None)` means the timeout elapsed (or the helper's stdout disconnected).
/// The mutex guarding the process is never held while this blocks.
pub fn next_event(timeout_ms: u32) -> Result<Option<Value>, String> {
    ensure_started()?;

    // Events stashed by `status` take priority to preserve ordering.
    {
        let mut pending = PENDING
            .lock()
            .map_err(|e| format!("watcher_events pending lock envenenado: {e}"))?;
        if let Some(value) = pending.pop_front() {
            return Ok(Some(value));
        }
    }

    // Take the receiver out, release the lock, then block on recv_timeout.
    let rx = {
        let mut guard = EVENT_RX
            .lock()
            .map_err(|e| format!("watcher_events event lock envenenado: {e}"))?;
        match guard.take() {
            Some(rx) => rx,
            None => return Ok(None), // another caller currently owns it
        }
    };

    let received = rx.recv_timeout(Duration::from_millis(timeout_ms as u64));

    // Park it back for the next call, even on disconnect/timeout.
    {
        let mut guard = EVENT_RX
            .lock()
            .map_err(|e| format!("watcher_events event lock envenenado: {e}"))?;
        *guard = Some(rx);
    }

    match received {
        Ok(value) => Ok(Some(value)),
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => Ok(None),
    }
}

/// Tears down the long-lived helper. Safe to call more than once.
pub fn shutdown() {
    if let Ok(mut guard) = proc_lock() {
        if let Some(mut proc) = guard.take() {
            let _ = writeln!(proc.stdin, "quit");
            let _ = proc.stdin.flush();
            let _ = proc.child.wait();
        }
    }
    if let Ok(mut rx) = EVENT_RX.lock() {
        *rx = None;
    }
    if let Ok(mut pending) = PENDING.lock() {
        pending.clear();
    }
    if let Ok(mut ring) = NATIVE_RING.lock() {
        ring.clear();
    }
}
