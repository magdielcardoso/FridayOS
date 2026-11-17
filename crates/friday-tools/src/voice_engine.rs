//! JARVIS-style audio foundations, exposed as a thin wrapper over two tiny
//! Swift helpers (see `swift/voice_engine.swift` and `swift/sound.swift`).
//!
//! `voice_engine.swift` is a long-lived process that reads commands from stdin
//! and emits one JSON object per line on stdout. We keep a single instance in
//! a global mutex: commands are written to its stdin and we block until the
//! matching terminal event arrives. `sound.swift` is a one-shot helper run via
//! the usual cached-binary mechanism.
//!
//! The helper is compiled lazily and cached under
//! `~/Library/Caches/FridayOS/bin` by [`crate::swift::helper`].
//!
//! NOTE: this module is intentionally standalone. Wire it into the crate by
//! adding `pub mod voice_engine;` to `src/lib.rs` from the integrator.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

struct EngineProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One long-lived helper for the whole process. `Option` lets it be spawned on
/// first use and respawned if it ever dies.
static ENGINE: Mutex<Option<EngineProcess>> = Mutex::new(None);
/// Separate long-lived helper for the ambient music bed.
static MUSIC: Mutex<Option<EngineProcess>> = Mutex::new(None);

fn lock_engine() -> Result<MutexGuard<'static, Option<EngineProcess>>, String> {
    ENGINE.lock().map_err(|e| format!("voice_engine lock envenenado: {e}"))
}

fn ensure_engine<'a>(
    guard: &'a mut MutexGuard<'static, Option<EngineProcess>>,
) -> Result<&'a mut EngineProcess, String> {
    if guard.is_none() {
        let binary = crate::swift::helper("voice_engine").map_err(|e| e.to_string())?;
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Never leave an undrained stderr pipe that could fill and block.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("voice_engine helper: {e}"))?;
        let stdin = child.stdin.take().ok_or("voice_engine: stdin indisponível")?;
        let stdout = child.stdout.take().ok_or("voice_engine: stdout indisponível")?;
        **guard = Some(EngineProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        });
    }
    Ok(guard.as_mut().expect("helper acabado de iniciar"))
}

fn send(proc: &mut EngineProcess, command: &str) -> Result<(), String> {
    proc.stdin
        .write_all(command.as_bytes())
        .and_then(|_| proc.stdin.write_all(b"\n"))
        .and_then(|_| proc.stdin.flush())
        .map_err(|e| format!("voice_engine stdin: {e}"))
}

/// Reads JSON lines until one matches `wanted` or is an error event. Ignores
/// blank lines and non-JSON chatter so the protocol stays robust.
fn wait_for_event(proc: &mut EngineProcess, wanted: &[&str]) -> Result<Value, String> {
    loop {
        let mut line = String::new();
        let read = proc
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("voice_engine stdout: {e}"))?;
        if read == 0 {
            return Err("voice_engine: processo encerrou antes do evento esperado".to_string());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let event = value.get("event").and_then(Value::as_str).unwrap_or("");
        if event == "error" {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.get("error").and_then(Value::as_str))
                .unwrap_or("erro desconhecido do helper");
            return Err(message.to_string());
        }
        if wanted.contains(&event) {
            return Ok(value);
        }
    }
}

/// Engine and voice-processing (AEC) status as a JSON value.
pub fn status() -> Result<Value, String> {
    let mut guard = lock_engine()?;
    let proc = ensure_engine(&mut guard)?;
    send(proc, "status")?;
    wait_for_event(proc, &["status"])
}

/// Starts capturing the microphone into `path` (16 kHz mono PCM16 WAV).
/// The WAV is finalized by [`stop_capture`].
pub fn start_capture(path: &Path) -> Result<(), String> {
    let command = format!("capture-start {}", path.display());
    let mut guard = lock_engine()?;
    let proc = ensure_engine(&mut guard)?;
    send(proc, &command)?;
    wait_for_event(proc, &["started"])?;
    Ok(())
}

/// Stops the active capture and returns the path of the finalized WAV.
pub fn stop_capture() -> Result<PathBuf, String> {
    let mut guard = lock_engine()?;
    let proc = match guard.as_mut() {
        Some(proc) => proc,
        None => return Err("voice_engine: nenhuma captura ativa".to_string()),
    };
    send(proc, "capture-stop")?;
    let event = wait_for_event(proc, &["stopped"])?;
    let path = event
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "voice_engine: evento `stopped` sem caminho".to_string())?;
    Ok(PathBuf::from(path))
}

/// Plays a WAV through the engine's player node and waits for completion.
pub fn play(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("arquivo de áudio não encontrado: {}", path.display()));
    }
    let command = format!("play {}", path.display());
    let mut guard = lock_engine()?;
    let proc = ensure_engine(&mut guard)?;
    send(proc, &command)?;
    wait_for_event(proc, &["play-done"])?;
    Ok(())
}

/// Plays one of the locally synthesized UI sounds: wake, confirm, error,
/// thinking or done. Uses the one-shot `sound` helper.
pub fn ui_sound(name: &str) -> Result<(), String> {
    const NAMES: &[&str] = &["wake", "confirm", "error", "thinking", "done"];
    if !NAMES.contains(&name) {
        return Err(format!("som de UI desconhecido: {name}"));
    }
    let raw = crate::swift::run("sound", &["play", name]).map_err(|e| e.to_string())?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("done").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return Err(error.to_string());
        }
    }
    Err("sound helper não confirmou a reprodução".to_string())
}

fn lock_music() -> Result<MutexGuard<'static, Option<EngineProcess>>, String> {
    MUSIC
        .lock()
        .map_err(|error| format!("music lock envenenado: {error}"))
}

fn ensure_music<'a>(
    guard: &'a mut MutexGuard<'static, Option<EngineProcess>>,
) -> Result<&'a mut EngineProcess, String> {
    if guard.is_none() {
        let binary = crate::swift::helper("music").map_err(|error| error.to_string())?;
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("music helper: {error}"))?;
        let stdin = child.stdin.take().ok_or("music: stdin indisponível")?;
        let stdout = child.stdout.take().ok_or("music: stdout indisponível")?;
        **guard = Some(EngineProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        });
    }
    Ok(guard.as_mut().expect("music helper acabado de iniciar"))
}

fn music_command(command: &str, wanted: &[&str]) -> Result<Value, String> {
    let mut guard = lock_music()?;
    let proc = ensure_music(&mut guard)?;
    send(proc, command)?;
    wait_for_event(proc, wanted)
}

/// Starts the ambient bed. `path` overrides FRIDAY_MUSIC_FILE; with neither,
/// a quiet procedural pad is generated locally.
pub fn music_start(path: Option<&Path>) -> Result<(), String> {
    let configured = std::env::var_os("FRIDAY_MUSIC_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let command = match path.or(configured.as_deref()) {
        Some(path) => format!("start {}", path.display()),
        None => "start".to_string(),
    };
    music_command(&command, &["started"])?;
    Ok(())
}

pub fn music_stop() -> Result<(), String> {
    match MUSIC.lock() {
        Ok(mut guard) if guard.is_some() => {
            let proc = guard.as_mut().expect("music helper");
            send(proc, "stop")?;
            wait_for_event(proc, &["stopped"])?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Lowers the bed while Friday speaks and restores it afterwards.
pub fn music_duck(on: bool) -> Result<(), String> {
    // Never spawn the music helper just to duck a bed that was never started.
    if MUSIC.lock().map(|guard| guard.is_none()).unwrap_or(true) {
        return Ok(());
    }
    music_command(if on { "duck on" } else { "duck off" }, &["duck"])?;
    Ok(())
}

pub fn music_status() -> Result<Value, String> {
    music_command("status", &["status"])
}

/// Tears down the long-lived helper. Safe to call more than once.
pub fn shutdown() {
    if let Ok(mut guard) = lock_engine() {
        if let Some(mut proc) = guard.take() {
            let _ = send(&mut proc, "quit");
            let _ = proc.child.wait();
        }
    }
    if let Ok(mut guard) = lock_music() {
        if let Some(mut proc) = guard.take() {
            let _ = send(&mut proc, "quit");
            let _ = proc.child.wait();
        }
    }
}
