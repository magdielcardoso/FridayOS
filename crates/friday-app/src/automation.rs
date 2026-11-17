//! Local automation: webhooks and scheduled routines that wake Friday without
//! the user typing anything.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub struct Trigger {
    pub source: String,
    pub prompt: String,
}

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn read_json(path: PathBuf, fallback: Value) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(fallback)
}

fn write_json(path: PathBuf, value: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_vec_pretty(value) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn date_part(format: &str) -> String {
    // Bounded like every other subprocess in this module. `run_capture` drains
    // stdout while the child runs, so `/bin/date` can never wedge a tick.
    run_capture("/bin/date", &[format], Duration::from_secs(2))
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn hook_port() -> u16 {
    std::env::var("FRIDAY_HOOK_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8787)
}

/// Starts the HTTP listener and the routine scheduler. `push` receives every
/// event that should become a prompt for the agent.
pub fn start(port: u16, push: Arc<dyn Fn(Trigger) + Send + Sync>) {
    // Webhooks
    if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
        let push = Arc::clone(&push);
        std::thread::Builder::new()
            .name("friday-hooks".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let push = Arc::clone(&push);
                    std::thread::spawn(move || handle_hook(stream, push));
                }
            })
            .ok();
    }

    // Scheduler + system watchers + built-in watchdog/digest routines.
    // Written by this very thread, which lives inside the app process: a stale
    // heartbeat means the scheduler (and therefore the app) stopped ticking.
    std::thread::Builder::new()
        .name("friday-routines".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            write_heartbeat();
            run_due_routines(&push);
            run_due_watchers(&push);
            run_self_heal(&push);
            run_nightly_digest(&push);
            run_morning_briefing(&push);
        })
        .ok();
}

fn handle_hook(mut stream: TcpStream, push: Arc<dyn Fn(Trigger) + Send + Sync>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    let mut secret = String::new();
    let mut host = String::new();
    let mut origin = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        // Lowercase only the header NAME: lowercasing the value made every
        // secret with an uppercase letter fail forever.
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "x-friday-secret" => secret = value.trim().to_string(),
            "host" => host = value.trim().to_string(),
            "origin" => origin = value.trim().to_string(),
            _ => {}
        }
    }

    // A webhook is server-to-server. Browsers always send Origin on a
    // cross-origin POST, and a DNS-rebinding page reaches us with its own Host.
    if !origin.is_empty() {
        let _ = stream.write_all(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    if !host.is_empty()
        && !(host.starts_with("127.0.0.1")
            || host.starts_with("localhost")
            || host.starts_with("[::1]"))
    {
        let _ = stream.write_all(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }

    let mut body = vec![0u8; content_length.min(1_000_000)];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let name = path.trim_start_matches('/').trim_start_matches("hook/").to_string();
    let respond = |stream: &mut TcpStream, status: &str, body: &str| {
        let payload = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(payload.as_bytes());
    };

    if method != "POST" && method != "GET" {
        respond(&mut stream, "405 Method Not Allowed", r#"{"ok":false}"#);
        return;
    }
    if name.is_empty() {
        respond(&mut stream, "404 Not Found", r#"{"ok":false,"error":"use /hook/<nome>"}"#);
        return;
    }

    let hooks = read_json(data_dir().join("webhooks.json"), json!([]));
    let hook = hooks
        .as_array()
        .and_then(|items| {
            items.iter().find(|hook| {
                hook.get("name").and_then(|value| value.as_str()) == Some(name.as_str())
            })
        })
        .cloned();

    let Some(hook) = hook else {
        respond(&mut stream, "404 Not Found", r#"{"ok":false,"error":"webhook desconhecido"}"#);
        return;
    };
    let expected = hook
        .get("secret")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if expected.is_empty() {
        respond(
            &mut stream,
            "403 Forbidden",
            r#"{"ok":false,"error":"webhook sem secret; crie outro com webhook_create"}"#,
        );
        return;
    }
    if !secret_eq(expected, &secret) {
        respond(&mut stream, "403 Forbidden", r#"{"ok":false,"error":"secret inválido"}"#);
        return;
    }

    let template = hook.get("prompt").and_then(|value| value.as_str()).unwrap_or("");
    let payload_note = if body.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nPayload recebido:\n{body}")
    };
    push(Trigger {
        source: format!("webhook:{name}"),
        prompt: format!("{template}{payload_note}"),
    });
    respond(&mut stream, "200 OK", r#"{"ok":true}"#);
}

fn run_due_routines(push: &Arc<dyn Fn(Trigger) + Send + Sync>) {
    let path = data_dir().join("routines.json");
    let data = read_json(path.clone(), json!([]));
    let Some(routines) = data.as_array() else {
        return;
    };
    if routines.is_empty() {
        return;
    }
    let mut changed = false;
    let now = now_secs();
    let hhmm = date_part("+%H:%M");
    let date = date_part("+%Y-%m-%d");
    let weekday = date_part("+%u");

    let mut updated: Vec<Value> = Vec::new();
    for routine in routines {
        let mut routine = routine.clone();
        let enabled = routine
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if !enabled {
            updated.push(routine);
            continue;
        }
        let schedule = routine.get("schedule").cloned().unwrap_or(Value::Null);
        let kind = schedule.get("type").and_then(|value| value.as_str()).unwrap_or("");
        let last_run = routine.get("last_run").and_then(|value| value.as_u64()).unwrap_or(0);
        let last_day = routine
            .get("last_day")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let fired = routine.get("fired").and_then(|value| value.as_bool()).unwrap_or(false);

        let due = match kind {
            "interval" => {
                let minutes = schedule.get("minutes").and_then(|value| value.as_u64()).unwrap_or(60);
                now.saturating_sub(last_run) >= minutes * 60
            }
            "daily" => {
                let time = schedule.get("time").and_then(|value| value.as_str()).unwrap_or("");
                time == hhmm && last_day != date
            }
            "weekly" => {
                let time = schedule.get("time").and_then(|value| value.as_str()).unwrap_or("");
                let day = schedule.get("weekday").and_then(|value| value.as_str()).unwrap_or("");
                time == hhmm && day == weekday && last_day != date
            }
            "at" => {
                let at = schedule.get("epoch").and_then(|value| value.as_u64()).unwrap_or(u64::MAX);
                !fired && now >= at
            }
            _ => false,
        };

        if due {
            let name = routine.get("name").and_then(|value| value.as_str()).unwrap_or("rotina");
            let prompt = routine.get("prompt").and_then(|value| value.as_str()).unwrap_or("");
            push(Trigger {
                source: format!("rotina:{name}"),
                prompt: prompt.to_string(),
            });
            routine["last_run"] = json!(now);
            routine["last_day"] = json!(date);
            if kind == "at" {
                routine["fired"] = json!(true);
                routine["enabled"] = json!(false);
            }
            changed = true;
        }
        updated.push(routine);
    }

    if changed {
        // Merge with the file as it is now: watchers created or deleted while
        // the sources were being read (up to 25 s each) used to be reverted.
        let updates: std::collections::HashMap<String, Value> = updated
            .into_iter()
            .filter_map(|entry| {
                let name = entry.get("name").and_then(|value| value.as_str())?.to_string();
                Some((name, entry))
            })
            .collect();
        let current = read_json(path.clone(), json!([]));
        let merged: Vec<Value> = current
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let name = item.get("name").and_then(|value| value.as_str()).unwrap_or("");
                        updates.get(name).cloned().unwrap_or_else(|| item.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        write_json(path, &Value::Array(merged));
    }
}

// ---------------------------------------------------------------------------
// System watchers: shell output, files, URLs, clipboard, iMessage, Mail.
// ---------------------------------------------------------------------------

/// Length-aware comparison that does not exit on the first mismatching byte.
fn secret_eq(expected: &str, got: &str) -> bool {
    let (a, b) = (expected.as_bytes(), got.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn signature(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn run_capture(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    // Drain the pipe while the child runs. Reading only after exit deadlocked
    // as soon as the command wrote more than one pipe buffer (64 KB).
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut buffer = String::new();
            let _ = stdout.read_to_string(&mut buffer);
            buffer
        })
    });
    let started = SystemTime::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
        if started.elapsed().unwrap_or_default() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("tempo esgotado".into());
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let _ = status;
    Ok(reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default())
}

/// Reads one source and returns (signature, human detail).
fn read_source(source: &str, params: &Value) -> Result<(String, String), String> {
    read_source_with_cap(source, params, None)
}

/// Same as [`read_source`], but `cap` overrides (lowers) every subprocess
/// timeout. Composite watchers pass a short cap so 4 probes stay bounded.
fn read_source_with_cap(
    source: &str,
    params: &Value,
    cap: Option<Duration>,
) -> Result<(String, String), String> {
    let bounded = |default: u64| cap.unwrap_or_else(|| Duration::from_secs(default));
    match source {
        "shell" => {
            let command = params
                .get("command")
                .and_then(|value| value.as_str())
                .ok_or("watcher shell sem `command`")?;
            let output = run_capture("/bin/zsh", &["-lc", command], bounded(20))?;
            let trimmed: String = output.chars().take(2000).collect();
            Ok((signature(&output), trimmed))
        }
        "file" => {
            let path = params
                .get("path")
                .and_then(|value| value.as_str())
                .ok_or("watcher file sem `path`")?;
            let expanded = if let Some(rest) = path.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{rest}")
            } else {
                path.to_string()
            };
            let metadata = std::fs::metadata(&expanded).map_err(|error| error.to_string())?;
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let detail = format!(
                "{} · {} bytes · modificado em {}",
                expanded, metadata.len(), modified
            );
            Ok((signature(&format!("{modified}:{}", metadata.len())), detail))
        }
        "http" => {
            let url = params
                .get("url")
                .and_then(|value| value.as_str())
                .ok_or("watcher http sem `url`")?;
            let max_time = bounded(20).as_secs().to_string();
            let output = run_capture(
                "/usr/bin/curl",
                &["-sL", "--max-time", &max_time, "-A", "FridayOS/0.1", url],
                cap.unwrap_or_else(|| Duration::from_secs(25)),
            )?;
            let trimmed: String = output.chars().take(2000).collect();
            Ok((signature(&output), trimmed))
        }
        "clipboard" => {
            let output = run_capture("/usr/bin/pbpaste", &[], bounded(5))?;
            let trimmed: String = output.chars().take(2000).collect();
            Ok((signature(&output), trimmed))
        }
        "imessage" => {
            // Needs Full Disk Access. Reads the newest messages, not the whole DB.
            let home = std::env::var("HOME").unwrap_or_default();
            let db = format!("{home}/Library/Messages/chat.db");
            let query = "SELECT ROWID, is_from_me, COALESCE(text,'') FROM message ORDER BY ROWID DESC LIMIT 5;";
            let output = run_capture("/usr/bin/sqlite3", &[&db, query], bounded(10))?;
            let max_row: String = output
                .lines()
                .filter_map(|line| line.split('|').next())
                .max_by_key(|row| row.parse::<u64>().unwrap_or(0))
                .unwrap_or("0")
                .to_string();
            let detail: String = output.chars().take(1200).collect();
            Ok((format!("imessage:{max_row}"), detail))
        }
        "email" => {
            // Mail.app via JXA; count + newest subject is enough to detect change.
            let script = "const Mail = Application('Mail'); const inbox = Mail.inbox; const msgs = inbox.messages; JSON.stringify({count: msgs.length, subject: msgs.length ? msgs[0].subject() : '', sender: msgs.length ? (msgs[0].sender() || '') : ''});";
            let output = run_capture(
                "/usr/bin/osascript",
                &["-l", "JavaScript", "-e", script],
                bounded(25),
            )?;
            Ok((signature(&output), output.trim().to_string()))
        }
        other => Err(format!("fonte desconhecida: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Temporal patterns & composite sources (all state lives in watchers.json).
// ---------------------------------------------------------------------------

const MAX_SIGNATURE_HISTORY: usize = 20;
const DEFAULT_REPEAT_WINDOW_SECS: u64 = 300;

fn parse_history(watcher: &Value) -> Vec<(u64, String)> {
    watcher
        .get("last_signatures")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let ts = item.get("ts")?.as_u64()?;
                    let sig = item.get("sig")?.as_str()?.to_string();
                    Some((ts, sig))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Append one observation and keep the rolling buffer strictly bounded.
/// Returns `true` when the new observation added information (not just a
/// duplicate of the latest signature).
fn pulse_history(history: &mut Vec<(u64, String)>, now: u64, sig: &str) -> bool {
    let appended_duplicate = history.last().map(|entry| entry.1.as_str()) == Some(sig);
    history.push((now, sig.to_string()));
    if history.len() > MAX_SIGNATURE_HISTORY {
        let excess = history.len() - MAX_SIGNATURE_HISTORY;
        history.drain(0..excess);
    }
    !appended_duplicate
}

fn history_value(history: &[(u64, String)]) -> Value {
    Value::Array(
        history
            .iter()
            .map(|(ts, sig)| json!({ "ts": ts, "sig": sig }))
            .collect(),
    )
}

/// How many trailing observations (inside `window`) carry `sig`.
fn trailing_same_count(history: &[(u64, String)], now: u64, sig: &str, window: u64) -> usize {
    let cutoff = now.saturating_sub(window);
    let mut count = 0usize;
    for (ts, observed) in history.iter().rev() {
        if *ts < cutoff {
            break;
        }
        if observed != sig {
            break;
        }
        count += 1;
    }
    count
}

/// Number of signature changes inside `window` (flap detector).
fn transition_count(history: &[(u64, String)], now: u64, window: u64) -> usize {
    let cutoff = now.saturating_sub(window);
    let mut previous: Option<&str> = None;
    let mut changes = 0usize;
    for (ts, sig) in history.iter() {
        if *ts < cutoff {
            continue;
        }
        if let Some(previous) = previous {
            if previous != sig.as_str() {
                changes += 1;
            }
        }
        previous = Some(sig.as_str());
    }
    changes
}

/// Evaluate a `composite` watcher: probe up to 4 children with short timeouts
/// and return (overall signature, human detail, child signatures). The overall
/// signature only encodes the *combined decision*, so a single child changing
/// cannot retrigger an `and` before every child agrees.
fn read_composite(
    params: &Value,
    previous_sigs: &[String],
    now: u64,
    budget: &mut usize,
) -> Result<(String, String, Vec<String>), String> {
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("or")
        .to_ascii_lowercase();
    if op != "and" && op != "or" {
        return Err(format!("composite `op` inválido: {op} (use and ou or)"));
    }
    let children = params
        .get("watchers")
        .and_then(Value::as_array)
        .ok_or("watcher composite sem `watchers`")?;
    if children.is_empty() {
        return Err("watcher composite com `watchers` vazio".into());
    }

    let cap = Duration::from_secs(5);
    let total = children.iter().take(4).count();
    let mut sigs = Vec::new();
    let mut detail_lines = Vec::new();
    let mut changed_flags = Vec::new();
    let mut probed = 0usize;

    for (index, child) in children.iter().take(4).enumerate() {
        let source = child.get("source").and_then(Value::as_str).unwrap_or("");
        let child_params = child.get("params").cloned().unwrap_or(Value::Null);
        let previous = previous_sigs.get(index).cloned().unwrap_or_default();

        let (sig, note) = if *budget == 0 {
            (previous.clone(), "orçamento de sondas esgotado".to_string())
        } else {
            *budget -= 1;
            probed += 1;
            match read_source_with_cap(source, &child_params, Some(cap)) {
                Ok((sig, detail)) => {
                    let trimmed: String = detail.chars().take(300).collect();
                    (sig, trimmed)
                }
                Err(error) => (previous.clone(), format!("erro: {error}")),
            }
        };

        let changed = !previous.is_empty() && previous != sig;
        changed_flags.push(changed);
        sigs.push(sig);
        detail_lines.push(format!(
            "[{index}] {source}: {} · {}",
            if changed { "mudou" } else { "estável" },
            note
        ));
    }

    let changed_count = changed_flags.iter().filter(|flag| **flag).count();
    let combined = if op == "and" {
        total > 0 && changed_count == total
    } else {
        changed_count > 0
    };
    let combined_sig = format!("composite:{op}:{combined}");
    let detail = format!(
        "composto ({op}): {changed_count}/{total} filhos mudaram ({probed} sondados) em {now}\n{}",
        detail_lines.join("\n")
    );
    Ok((signature(&combined_sig), detail, sigs))
}

fn run_due_watchers(push: &Arc<dyn Fn(Trigger) + Send + Sync>) {
    let path = data_dir().join("watchers.json");
    let data = read_json(path.clone(), json!([]));
    let Some(watchers) = data.as_array() else {
        return;
    };
    if watchers.is_empty() {
        return;
    }
    let now = now_secs();
    let mut updated: Vec<Value> = Vec::new();
    let mut changed = false;
    // Shared across the whole tick: never more than 4 composite child probes.
    let mut composite_budget = 4usize;

    for watcher in watchers {
        let mut watcher = watcher.clone();
        let enabled = watcher.get("enabled").and_then(|value| value.as_bool()).unwrap_or(true);
        let interval = watcher
            .get("interval_secs")
            .and_then(|value| value.as_u64())
            .unwrap_or(60)
            .max(15);
        let last_check = watcher.get("last_check").and_then(|value| value.as_u64()).unwrap_or(0);
        if !enabled || now.saturating_sub(last_check) < interval {
            updated.push(watcher);
            continue;
        }

        let source = watcher.get("source").and_then(|value| value.as_str()).unwrap_or("").to_string();
        let params = watcher.get("params").cloned().unwrap_or(Value::Null);
        let name = watcher.get("name").and_then(|value| value.as_str()).unwrap_or("watcher").to_string();
        let mut history = parse_history(&watcher);
        let previous = watcher
            .get("last_value")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let first = previous.is_empty();

        let probed = if source == "composite" {
            let previous_sigs: Vec<String> = watcher
                .get("child_signatures")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default();
            match read_composite(&params, &previous_sigs, now, &mut composite_budget) {
                Ok((sig, detail, child_sigs)) => {
                    watcher["child_signatures"] = json!(child_sigs);
                    Ok((sig, detail))
                }
                Err(error) => Err(error),
            }
        } else {
            read_source(&source, &params)
        };

        match probed {
            Ok((sig, detail)) => {
                pulse_history(&mut history, now, &sig);
                watcher["last_signatures"] = history_value(&history);

                let fire_on_start = watcher
                    .get("fire_on_start")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let contains_ok = params
                    .get("contains")
                    .and_then(|value| value.as_str())
                    .map(|needle| detail.to_lowercase().contains(&needle.to_lowercase()))
                    .unwrap_or(true);

                let window = params
                    .get("repeat_window_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_REPEAT_WINDOW_SECS)
                    .max(1);
                let repeat_n = params.get("repeat_n").and_then(Value::as_u64).filter(|n| *n >= 2);
                let flap_n = params.get("flap_n").and_then(Value::as_u64).filter(|n| *n >= 2);
                let quiet_secs = params.get("quiet_secs").and_then(Value::as_u64).unwrap_or(0);

                let stored_fired = watcher
                    .get("fired_sig")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let last_fire_ts = watcher.get("last_fire_ts").and_then(Value::as_u64).unwrap_or(0);
                let last_flap_fire = watcher.get("last_flap_fire").and_then(Value::as_u64).unwrap_or(0);
                // Legacy watchers have no `fired_sig`. Persist the baseline
                // once so repeat_n can distinguish a genuinely new signature
                // from the watcher's own last observation.
                let last_fired_sig = if stored_fired.is_empty() {
                    let baseline = if first { sig.as_str() } else { previous.as_str() };
                    watcher["fired_sig"] = json!(baseline);
                    baseline.to_string()
                } else {
                    stored_fired
                };

                let mut fire = false;
                if first {
                    fire = fire_on_start;
                } else {
                    let mut conditions: Vec<bool> = Vec::new();
                    if let Some(n) = repeat_n {
                        let trailing = trailing_same_count(&history, now, &sig, window);
                        conditions.push(trailing >= n as usize && sig != last_fired_sig);
                    }
                    if let Some(n) = flap_n {
                        let changes = transition_count(&history, now, window);
                        let cooldown_ok = now.saturating_sub(last_flap_fire) >= window;
                        conditions.push(changes >= n as usize && cooldown_ok);
                    }
                    if conditions.is_empty() {
                        // Default behavior is untouched: fire on any change.
                        fire = previous != sig;
                    } else {
                        fire = conditions.iter().all(|condition| *condition);
                    }
                }

                if fire
                    && quiet_secs > 0
                    && sig == last_fired_sig
                    && now.saturating_sub(last_fire_ts) < quiet_secs
                {
                    fire = false;
                }
                fire = fire && contains_ok;

                if fire {
                    let prompt = watcher.get("prompt").and_then(|value| value.as_str()).unwrap_or("");
                    // `source` is the "name" the app uses to route the trigger;
                    // mission watchers already carry `watcher:mission:<id>`.
                    push(Trigger {
                        source: format!("watcher:{name}"),
                        prompt: format!(
                            "{prompt}\n\nMudança detectada em `{name}` ({source}):\n{}",
                            detail.chars().take(1500).collect::<String>()
                        ),
                    });
                    watcher["fired_sig"] = json!(sig);
                    watcher["last_fire_ts"] = json!(now);
                    if flap_n.is_some() {
                        watcher["last_flap_fire"] = json!(now);
                    }
                }
                watcher["last_value"] = json!(sig);
                if first {
                    watcher["baseline_at"] = json!(now);
                }
            }
            Err(error) => {
                watcher["last_error"] = json!(error);
            }
        }
        watcher["last_check"] = json!(now);
        changed = true;
        updated.push(watcher);
    }

    if changed {
        // Merge-on-write: watchers created/deleted by friday-tools while the
        // probes were running must survive (same discipline as routines).
        let updates: std::collections::HashMap<String, Value> = updated
            .into_iter()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.to_string();
                Some((name, entry))
            })
            .collect();
        let current = read_json(path.clone(), json!([]));
        let merged: Vec<Value> = current
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        updates.get(name).cloned().unwrap_or_else(|| item.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        write_json(path, &Value::Array(merged));
    }
}

// ---------------------------------------------------------------------------
// Built-in self-heal ("watchdog") routine. It only reports — never restarts.
// ---------------------------------------------------------------------------

const AUTOHEAL_STATE_FILE: &str = "autoheal-state.json";
const AUTOHEAL_ALERT_GAP_SECS: u64 = 900;
const SELFHEAL_INTERVAL_SECS: u64 = 60;

fn heartbeat_path() -> PathBuf {
    data_dir().join("heartbeat.json")
}

/// Newest persisted Codex thread id (`thread-*.json` is written by the app on
/// `CodexEvent::Online`). Combined with heartbeat freshness it is the
/// "thread connected" indicator the watchdog reads.
fn current_thread_id() -> String {
    let mut best: Option<(SystemTime, String)> = None;
    if let Ok(entries) = std::fs::read_dir(data_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if !file_name.starts_with("thread-") || !file_name.ends_with(".json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            if best.as_ref().map(|(time, _)| modified > *time).unwrap_or(true) {
                best = Some((modified, id.to_string()));
            }
        }
    }
    best.map(|(_, id)| id).unwrap_or_default()
}

/// Written by the scheduler thread on every 5 s tick. A stale/absent file means
/// the app process stopped ticking.
fn write_heartbeat() {
    let payload = json!({
        "ts": now_secs(),
        "pid": std::process::id(),
        "thread": current_thread_id(),
    });
    write_json(heartbeat_path(), &payload);
}

fn mobile_port() -> u16 {
    std::env::var("FRIDAY_MOBILE_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3000)
}

fn log_autoheal(message: &str) {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = PathBuf::from(home).join("Library/Logs/FridayOS.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "[autoheal] {message}");
    }
}

fn alert_due(state: &Value, key: &str, now: u64) -> bool {
    state
        .get("alerts")
        .and_then(|alerts| alerts.get(key))
        .and_then(Value::as_u64)
        .map(|last| now.saturating_sub(last) >= AUTOHEAL_ALERT_GAP_SECS)
        .unwrap_or(true)
}

fn mark_alert(state: &mut Value, key: &str, now: u64) {
    if !state.is_object() {
        *state = json!({});
    }
    let alerts = state
        .as_object_mut()
        .expect("state is an object")
        .entry("alerts".to_string())
        .or_insert_with(|| json!({}));
    if let Some(map) = alerts.as_object_mut() {
        map.insert(key.to_string(), json!(now));
    }
}

fn run_self_heal(push: &Arc<dyn Fn(Trigger) + Send + Sync>) {
    let now = now_secs();
    let state_path = data_dir().join(AUTOHEAL_STATE_FILE);
    let mut state = read_json(state_path.clone(), json!({}));
    if !state.is_object() {
        state = json!({});
    }
    let last_check = state.get("last_check").and_then(Value::as_u64).unwrap_or(0);
    if now.saturating_sub(last_check) < SELFHEAL_INTERVAL_SECS {
        return;
    }
    state["last_check"] = json!(now);

    let heartbeat = read_json(heartbeat_path(), json!({}));
    let hb_ts = heartbeat.get("ts").and_then(Value::as_u64).unwrap_or(0);
    let pid = heartbeat.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
    let thread = heartbeat
        .get("thread")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // description, suggested repair
    let mut problems: Vec<(String, String)> = Vec::new();

    // 1. FridayOS process alive. Read-only `ps` probe (no signal is sent); the
    //    same output supplies the CPU sample below.
    let pid_text = pid.to_string();
    let ps_output = if pid == 0 {
        None
    } else {
        run_capture("/bin/ps", &["-p", &pid_text, "-o", "%cpu="], Duration::from_secs(3)).ok()
    };
    let process_alive = ps_output
        .as_ref()
        .map(|output| !output.trim().is_empty())
        .unwrap_or(false);
    if !process_alive {
        problems.push((
            "Processo FridayOS não está respondendo (pid do heartbeat ausente ou morto).".into(),
            "Abra o app com `open dist/FridayOS.app` (sem -n); não reinicie nada automaticamente."
                .into(),
        ));
    }

    // 2. Local port (3000 by default) answering — no subprocess needed.
    let port = mobile_port();
    let address: Option<SocketAddr> = format!("127.0.0.1:{port}").parse().ok();
    let port_ok = address
        .and_then(|address| TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok())
        .is_some();
    if !port_ok {
        problems.push((
            format!("Porta {port} não está respondendo."),
            "Verifique `mobile::start` em main.rs e o firewall local; sem reinício automático."
                .into(),
        ));
    }

    // 3. Codex thread connected indicator.
    let heartbeat_age = now.saturating_sub(hb_ts);
    if thread.is_empty() || heartbeat_age > 120 {
        let description = if thread.is_empty() {
            "Nenhuma thread do Codex conectada (heartbeat sem thread id).".to_string()
        } else {
            format!("Heartbeat do Codex antigo ({}s sem atualização).", heartbeat_age)
        };
        problems.push((
            description,
            "Use `friday__codex_accounts` / `friday__codex_login` e aguarde o evento Online; \
             não mate processos."
                .into(),
        ));
    }

    // 4. CPU > 90% sustained for 5 minutes (reuses the ps sample above).
    let cpu = ps_output
        .and_then(|output| output.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let mut cpu_high_since = state.get("cpu_high_since").and_then(Value::as_u64).unwrap_or(0);
    if cpu > 90.0 {
        if cpu_high_since == 0 {
            cpu_high_since = now;
        }
        if now.saturating_sub(cpu_high_since) >= 300 {
            problems.push((
                format!("CPU do FridayOS em {cpu:.0}% há mais de 5 minutos."),
                "Rode `diagnose` para achar o culpado e feche trabalho pesado; reinicie só com \
                 confirmação."
                    .into(),
            ));
        }
    } else {
        cpu_high_since = 0;
    }
    state["cpu_high_since"] = json!(cpu_high_since);

    // 5. Log growth > 20 MB since the previous check.
    let home = std::env::var_os("HOME").unwrap_or_default();
    let log_path = PathBuf::from(home).join("Library/Logs/FridayOS.log");
    let log_len = std::fs::metadata(&log_path).map(|metadata| metadata.len()).unwrap_or(0);
    let mut log_baseline = state.get("log_baseline").and_then(Value::as_u64).unwrap_or(0);
    if log_baseline == 0 {
        log_baseline = log_len;
    }
    let grown = log_len.saturating_sub(log_baseline);
    if grown > 20 * 1024 * 1024 {
        problems.push((
            format!("Log cresceu {} MB desde a última checagem.", grown / 1024 / 1024),
            "Rotacione/compacte ~/Library/Logs/FridayOS.log e investigue com `diagnose` antes \
             de apagar."
                .into(),
        ));
        log_baseline = log_len;
    }
    state["log_baseline"] = json!(log_baseline);

    for (description, repair) in problems {
        let key = signature(&description);
        if !alert_due(&state, &key, now) {
            continue;
        }
        mark_alert(&mut state, &key, now);
        let message = format!("{description} Sugestão: {repair}");
        log_autoheal(&message);
        // `source` is the delivery name the app routes on: watchdog:friday.
        push(Trigger {
            source: "watchdog:friday".into(),
            prompt: format!(
                "[autoheal][urgente] {message}\n\nInvestigue e corrija sem reiniciar ou matar \
                 processos por conta própria."
            ),
        });
    }

    write_json(state_path, &state);
}

// ---------------------------------------------------------------------------
// Nightly digest (03:00-04:00 local, once per day).
// ---------------------------------------------------------------------------

fn gather_missions() -> String {
    let mut entries: Vec<(u64, String)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(data_dir().join("missions")) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if !value.is_object() {
                continue;
            }
            let id = value
                .get("id")
                .or_else(|| value.get("slug"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            let name = value.get("name").and_then(Value::as_str).unwrap_or("");
            let goal: String = value
                .get("goal")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            let status = value.get("status").and_then(Value::as_str).unwrap_or("");
            let findings = value
                .get("findings")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            let created = value.get("created_at").and_then(Value::as_u64).unwrap_or(0);
            let name_part = if name.is_empty() {
                String::new()
            } else {
                format!(" ({name})")
            };
            entries.push((
                created,
                format!("- `{id}`{name_part}: {status} · achados={findings} · objetivo: {goal}"),
            ));
        }
    }
    entries.sort_by(|left, right| right.0.cmp(&left.0));
    if entries.is_empty() {
        "(nenhuma missão)".into()
    } else {
        entries
            .into_iter()
            .take(30)
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn gather_journal_tail() -> String {
    let path = data_dir().join("actions.jsonl");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return "(sem diário de ações)".into();
    };
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines.len().saturating_sub(200);
    if start == lines.len() {
        return "(vazio)".into();
    }
    lines[start..]
        .iter()
        .map(|line| line.chars().take(300).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn gather_trace_counts() -> (usize, usize) {
    let today = date_part("+%Y-%m-%d");
    let cutoff_ms: u128 = if today.is_empty() {
        (now_secs().saturating_sub(86_400) as u128) * 1000
    } else {
        let midnight = run_capture(
            "/bin/date",
            &["-j", "-f", "%Y-%m-%d", &today, "+%s"],
            Duration::from_secs(2),
        )
        .ok()
        .and_then(|output| output.trim().parse::<u64>().ok())
        .unwrap_or_else(|| now_secs().saturating_sub(86_400));
        (midnight as u128) * 1000
    };

    let mut traces = 0usize;
    let mut errors = 0usize;
    if let Ok(raw) = std::fs::read_to_string(data_dir().join("trace.jsonl")) {
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts = value.get("ts_ms").and_then(Value::as_u64).unwrap_or(0) as u128;
            if ts < cutoff_ms {
                continue;
            }
            traces += 1;
            if value.get("event").and_then(Value::as_str) == Some("error") {
                errors += 1;
            }
        }
    }
    (traces, errors)
}

fn run_nightly_digest(push: &Arc<dyn Fn(Trigger) + Send + Sync>) {
    let hour = date_part("+%H");
    if hour != "03" {
        return;
    }
    let date = date_part("+%Y-%m-%d");
    if date.is_empty() {
        return;
    }
    let state_path = data_dir().join("digest-state.json");
    let state = read_json(state_path.clone(), json!({}));
    if state.get("last_date").and_then(Value::as_str) == Some(date.as_str()) {
        return;
    }

    let missions = gather_missions();
    let journal = gather_journal_tail();
    let (traces, errors) = gather_trace_counts();
    let briefing = data_dir().join("briefing.md").to_string_lossy().to_string();

    let prompt = format!(
        "Resumo noturno de {date}. Você DEVE seguir estes passos, nesta ordem:\n\
         1. Chame `mission_sweep` e `timeline_query` PRIMEIRO para levantar o estado real das \
         missões e a linha do tempo de hoje.\n\
         2. Para cada missão ativa, chame `mission_update` com os achados encontrados (texto + \
         `state` mesclado). Se a missão estiver bloqueada ou precisar de decisão humana, chame \
         `mission_update` com `needs_approval`: true.\n\
         3. Quando a condição de conclusão de uma missão for satisfeita, chame `mission_complete` \
         com um achado final. Nunca deixe a missão ativa depois que a condição for atendida.\n\
         4. Escreva um briefing markdown curto em `{briefing}` com exatamente estas seções: \
         `## Missões`, `## Pendências`, `## Próximos passos`.\n\
         Não reinicie nem mate processos.\n\n\
         Missões ativas (máx. 30):\n{missions}\n\n\
         Diário de ações (últimas 200 linhas):\n{journal}\n\n\
         Traços de hoje: {traces} · erros de hoje: {errors}."
    );
    // `source` is the delivery name: routine:digest.
    push(Trigger {
        source: "routine:digest".into(),
        prompt,
    });
    write_json(
        state_path,
        &json!({ "last_date": date, "ran_at": now_secs() }),
    );
}

// ---------------------------------------------------------------------------
// Morning briefing (07:00-08:00 local, once per day). Same bounded pattern as
// the nightly digest, with its own state file. It only feeds a prompt to the
// agent — it never spawns a UI window or touches user state.
// ---------------------------------------------------------------------------

fn run_morning_briefing(push: &Arc<dyn Fn(Trigger) + Send + Sync>) {
    let hour = date_part("+%H");
    if hour != "07" {
        return;
    }
    let date = date_part("+%Y-%m-%d");
    if date.is_empty() {
        return;
    }
    let state_path = data_dir().join("briefing-morning-state.json");
    let state = read_json(state_path.clone(), json!({}));
    if state.get("last_date").and_then(Value::as_str) == Some(date.as_str()) {
        return;
    }

    let briefing = data_dir().join("briefing.md").to_string_lossy().to_string();
    let prompt = format!(
        "Briefing matinal de {date}. Chame `mission_sweep` para levantar o estado das missões, \
         leia o briefing noturno em `{briefing}` e consulte `calendar_today` para ver a agenda de \
         hoje. Então fale/mostre um resumo curto: missões ativas, pendências e os primeiros \
         compromissos do dia. Não abra janelas de UI, não reinicie nem mate processos."
    );
    // `source` is the delivery name: routine:briefing_morning.
    push(Trigger {
        source: "routine:briefing_morning".into(),
        prompt,
    });
    write_json(
        state_path,
        &json!({ "last_date": date, "ran_at": now_secs() }),
    );
}
