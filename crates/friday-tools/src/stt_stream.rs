//! Local streaming speech-to-text, backed by a long-lived Swift helper
//! (`swift/stt_stream.swift`).
//!
//! Friday's batch STT sends a whole WAV to Codex after VAD endpoint (accurate
//! but ~6 s late). This module is the fast, on-device complement: a continuous
//! `SFSpeechAudioBufferRecognitionRequest` tap emits [`SttEvent::Partial`] text
//! as the user speaks, which drives instant UI / turn-taking. The final,
//! high-fidelity transcript still comes from the batch pipeline.
//!
//! The helper is compiled lazily and cached under
//! `~/Library/Caches/FridayOS/bin` by [`crate::swift::helper`]. We keep a single
//! child in a global mutex holding only the process and its stdin; stdout is
//! drained by a dedicated background thread into an mpsc channel plus a bounded
//! ring (the ring backs [`last_partial`]). [`poll_events`] never holds a mutex
//! across a blocking receive: it briefly parks the channel receiver, does a
//! `recv_timeout`, then parks it back. No function panics.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A transcript event produced by the streaming recognizer. Control chatter
/// (`started` / `stopped`) is consumed internally by [`start`] / [`stop`].
#[derive(Clone, Debug)]
pub enum SttEvent {
    /// Best-effort transcript of the phrase in progress (may be revised).
    Partial(String),
    /// Recognizer-finalized transcript of a phrase.
    Final(String),
    /// Non-fatal helper/recognizer error (recognized phrase may continue).
    Error(String),
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
}

/// Guards the helper process and its stdin only. Never used for stdout reads.
static PROC: Mutex<Option<Proc>> = Mutex::new(None);
/// Parking spot for the event receiver. Held only while taking/putting the
/// receiver back - never while `recv_timeout` is blocking.
static EVENT_RX: Mutex<Option<Receiver<SttEvent>>> = Mutex::new(None);
/// Events taken out of order by a `start`/`stop` handshake are stashed here and
/// returned first by the next [`poll_events`], so nothing is lost.
static PENDING: Mutex<VecDeque<SttEvent>> = Mutex::new(VecDeque::new());
/// Bounded history of the most recent events (also the source of [`last_partial`]).
static RING: Mutex<VecDeque<SttEvent>> = Mutex::new(VecDeque::new());
const RING_CAP: usize = 50;

/// Whether a `start` has completed and a `stop`/disconnect has not happened.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Incremented on every helper spawn (and on shutdown) so a stale reader thread
/// cannot clear `RUNNING` after a new helper has taken over.
static GEN: AtomicU64 = AtomicU64::new(0);

const START_TIMEOUT_MS: u64 = 25_000; // first run may block ~20 s on TCC prompt
const STOP_TIMEOUT_MS: u64 = 5_000;

/// Control replies from the helper, waited on with a condvar so no mutex is
/// held across a blocking event-channel receive.
struct Control {
    started_seq: u64,
    stopped_seq: u64,
    last_error: Option<String>,
}
static CONTROL: Mutex<Control> = Mutex::new(Control {
    started_seq: 0,
    stopped_seq: 0,
    last_error: None,
});
static CONTROL_CV: Condvar = Condvar::new();

enum Kind {
    Started,
    Stopped,
    Error(String),
    Event(SttEvent),
}

fn parse_event(value: &Value) -> Option<Kind> {
    let event = value.get("event")?.as_str()?;
    let text = || value.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    match event {
        "started" => Some(Kind::Started),
        "stopped" => Some(Kind::Stopped),
        "error" => Some(Kind::Error(
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("erro desconhecido do helper")
                .to_string(),
        )),
        "partial" => Some(Kind::Event(SttEvent::Partial(text()))),
        "final" => Some(Kind::Event(SttEvent::Final(text()))),
        _ => None,
    }
}

fn notify_control<F: FnOnce(&mut Control)>(f: F) {
    if let Ok(mut control) = CONTROL.lock() {
        f(&mut control);
        CONTROL_CV.notify_all();
    }
}

fn lock_control() -> Result<MutexGuard<'static, Control>, String> {
    CONTROL.lock().map_err(|e| format!("stt_stream control lock envenenado: {e}"))
}

/// Records an event in the bounded ring and forwards it to the live channel.
/// Returns false when the receiver is gone (reader thread should stop).
fn dispatch(tx: &Sender<SttEvent>, event: SttEvent) -> bool {
    if let Ok(mut ring) = RING.lock() {
        if ring.len() >= RING_CAP {
            ring.pop_front();
        }
        ring.push_back(event.clone());
    }
    tx.send(event).is_ok()
}

fn reader_loop(stdout: ChildStdout, tx: Sender<SttEvent>, gen: u64) {
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
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };
        match parse_event(&value) {
            Some(Kind::Started) => {
                notify_control(|c| {
                    c.started_seq = c.started_seq.wrapping_add(1);
                    c.last_error = None;
                });
            }
            Some(Kind::Stopped) => {
                notify_control(|c| c.stopped_seq = c.stopped_seq.wrapping_add(1));
            }
            Some(Kind::Error(message)) => {
                notify_control(|c| c.last_error = Some(message.clone()));
                if !dispatch(&tx, SttEvent::Error(message)) {
                    break;
                }
            }
            Some(Kind::Event(event)) => {
                if !dispatch(&tx, event) {
                    break;
                }
            }
            None => {}
        }
    }

    // stdout disconnected: the helper exited. Only the current generation may
    // flip RUNNING / publish an error, or a stale reader would clobber a fresh
    // helper that was just spawned.
    if GEN.load(Ordering::SeqCst) == gen {
        RUNNING.store(false, Ordering::SeqCst);
        notify_control(|c| {
            c.last_error
                .get_or_insert_with(|| "helper stt_stream desconectado".to_string());
        });
    }
}

fn proc_lock() -> Result<MutexGuard<'static, Option<Proc>>, String> {
    PROC.lock().map_err(|e| format!("stt_stream lock envenenado: {e}"))
}

fn ensure_proc<'a>(guard: &'a mut MutexGuard<'static, Option<Proc>>) -> Result<&'a mut Proc, String> {
    // Reap a dead child and respawn on next use.
    let dead = match guard.as_mut() {
        Some(proc) => matches!(proc.child.try_wait(), Ok(Some(_))),
        None => false,
    };
    if dead {
        if let Some(mut proc) = guard.take() {
            let _ = proc.child.kill();
            let _ = proc.child.wait();
        }
    }

    if guard.is_none() {
        let binary = crate::swift::helper("stt_stream").map_err(|e| e.to_string())?;
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Never leave an undrained stderr pipe that could fill and block.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("stt_stream helper: {e}"))?;
        let stdin = child.stdin.take().ok_or("stt_stream: stdin indisponível")?;
        let stdout = child.stdout.take().ok_or("stt_stream: stdout indisponível")?;

        let (tx, rx) = mpsc::channel::<SttEvent>();
        let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::spawn(move || reader_loop(stdout, tx, gen));

        {
            let mut event_rx = EVENT_RX
                .lock()
                .map_err(|e| format!("stt_stream event lock envenenado: {e}"))?;
            *event_rx = Some(rx);
        }

        **guard = Some(Proc { child, stdin });
    }
    Ok(guard
        .as_mut()
        .ok_or_else(|| "stt_stream: helper indisponível".to_string())?)
}

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
        .map_err(|e| format!("stt_stream stdin: {e}"))
}

/// Takes one event, checking stashed events first. The receiver is removed from
/// its mutex before the blocking `recv_timeout` and parked back afterwards, so
/// no mutex is ever held across a blocking receive. `None` means timeout,
/// disconnect, or another caller currently owning the receiver.
fn next_raw(timeout_ms: u32) -> Option<SttEvent> {
    if let Ok(mut pending) = PENDING.lock() {
        if let Some(event) = pending.pop_front() {
            return Some(event);
        }
    }

    let rx = {
        let mut guard = EVENT_RX.lock().ok()?;
        guard.take()?
    };
    let received = rx.recv_timeout(Duration::from_millis(timeout_ms as u64));
    {
        if let Ok(mut guard) = EVENT_RX.lock() {
            *guard = Some(rx);
        }
    }
    received.ok()
}

/// Starts continuous on-device recognition. Idempotent while already running.
/// Waits for the helper's `started` reply; a permission/recognizer error from
/// the helper is returned as `Err`.
pub fn start() -> Result<(), String> {
    if is_running() {
        return Ok(());
    }
    ensure_started()?;

    let seq;
    {
        let mut control = lock_control()?;
        seq = control.started_seq;
        control.last_error = None; // discard stale errors from a prior session
    }
    send_command("start")?;

    let guard = lock_control()?;
    let (control, timed_out) = CONTROL_CV
        .wait_timeout_while(guard, Duration::from_millis(START_TIMEOUT_MS), |c| {
            c.started_seq == seq && c.last_error.is_none()
        })
        .map_err(|e| format!("stt_stream control lock envenenado: {e}"))?;
    if let Some(message) = control.last_error.as_ref() {
        return Err(message.clone());
    }
    if timed_out.timed_out() {
        return Err("stt_stream: timeout aguardando started".to_string());
    }
    RUNNING.store(true, Ordering::SeqCst);
    Ok(())
}

/// Stops recognition: cancels the task, removes the tap and stops the engine on
/// the helper side. A no-op when no helper exists. Waits for `stopped`.
pub fn stop() -> Result<(), String> {
    {
        let guard = proc_lock()?;
        if guard.is_none() {
            return Ok(());
        }
    }
    let seq;
    {
        let mut control = lock_control()?;
        seq = control.stopped_seq;
        control.last_error = None;
    }
    send_command("stop")?;

    let guard = lock_control()?;
    let (control, timed_out) = CONTROL_CV
        .wait_timeout_while(guard, Duration::from_millis(STOP_TIMEOUT_MS), |c| {
            c.stopped_seq == seq && c.last_error.is_none()
        })
        .map_err(|e| format!("stt_stream control lock envenenado: {e}"))?;
    if let Some(message) = control.last_error.as_ref() {
        return Err(message.clone());
    }
    if timed_out.timed_out() {
        return Err("stt_stream: timeout aguardando stopped".to_string());
    }
    RUNNING.store(false, Ordering::SeqCst);
    Ok(())
}

/// Whether a `start` has completed and no `stop`/disconnect has followed.
pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Collects every event that arrives within `timeout_ms` (waiting up to that
/// long for the first one, then draining what is queued). Never holds a mutex
/// across the blocking receive.
pub fn poll_events(timeout_ms: u32) -> Vec<SttEvent> {
    let mut events = Vec::new();
    // Out-of-order events stashed by a handshake are always "pending", even
    // with a zero budget, so drain them first.
    if let Ok(mut pending) = PENDING.lock() {
        events.extend(pending.drain(..));
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let remaining_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
        match next_raw(remaining_ms) {
            Some(event) => events.push(event),
            // Channel empty, disconnected, or owned by another caller.
            None => break,
        }
    }
    events
}

/// Most recent partial transcript still retained in the bounded ring, if any.
pub fn last_partial() -> Option<String> {
    let ring = RING.lock().ok()?;
    ring.iter()
        .rev()
        .find_map(|event| match event {
            SttEvent::Partial(text) => Some(text.clone()),
            _ => None,
        })
}

/// Tears down the long-lived helper and clears all buffered state. Safe to call
/// more than once.
pub fn shutdown() {
    GEN.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut guard) = proc_lock() {
        if let Some(mut proc) = guard.take() {
            let _ = writeln!(proc.stdin, "quit");
            let _ = proc.stdin.flush();
            let _ = proc.child.wait();
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
    if let Ok(mut event_rx) = EVENT_RX.lock() {
        *event_rx = None;
    }
    if let Ok(mut pending) = PENDING.lock() {
        pending.clear();
    }
    if let Ok(mut ring) = RING.lock() {
        ring.clear();
    }
    if let Ok(mut control) = CONTROL.lock() {
        control.last_error = None;
        CONTROL_CV.notify_all();
    }
}
