//! Local automation: webhooks and scheduled routines that wake Friday without
//! the user typing anything.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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
    std::process::Command::new("/bin/date")
        .arg(format)
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
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

    // Scheduler + system watchers
    std::thread::Builder::new()
        .name("friday-routines".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            run_due_routines(&push);
            run_due_watchers(&push);
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
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let lower = line.to_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if let Some(value) = lower.strip_prefix("x-friday-secret:") {
            secret = value.trim().to_string();
        }
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
    if let Some(expected) = hook.get("secret").and_then(|value| value.as_str()) {
        if !expected.is_empty() && expected != secret {
            respond(&mut stream, "403 Forbidden", r#"{"ok":false,"error":"secret inválido"}"#);
            return;
        }
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
        write_json(path, &Value::Array(updated));
    }
}

// ---------------------------------------------------------------------------
// System watchers: shell output, files, URLs, clipboard, iMessage, Mail.
// ---------------------------------------------------------------------------

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
    let started = SystemTime::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
        if started.elapsed().unwrap_or_default() > timeout {
            let _ = child.kill();
            return Err("tempo esgotado".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut output);
    }
    Ok(output)
}

/// Reads one source and returns (signature, human detail).
fn read_source(source: &str, params: &Value) -> Result<(String, String), String> {
    match source {
        "shell" => {
            let command = params
                .get("command")
                .and_then(|value| value.as_str())
                .ok_or("watcher shell sem `command`")?;
            let output = run_capture("/bin/zsh", &["-lc", command], Duration::from_secs(20))?;
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
            let output = run_capture(
                "/usr/bin/curl",
                &["-sL", "--max-time", "20", "-A", "FridayOS/0.1", url],
                Duration::from_secs(25),
            )?;
            let trimmed: String = output.chars().take(2000).collect();
            Ok((signature(&output), trimmed))
        }
        "clipboard" => {
            let output = run_capture("/usr/bin/pbpaste", &[], Duration::from_secs(5))?;
            let trimmed: String = output.chars().take(2000).collect();
            Ok((signature(&output), trimmed))
        }
        "imessage" => {
            // Needs Full Disk Access. Reads the newest messages, not the whole DB.
            let home = std::env::var("HOME").unwrap_or_default();
            let db = format!("{home}/Library/Messages/chat.db");
            let query = "SELECT ROWID, is_from_me, COALESCE(text,'') FROM message ORDER BY ROWID DESC LIMIT 5;";
            let output = run_capture("/usr/bin/sqlite3", &[&db, query], Duration::from_secs(10))?;
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
                Duration::from_secs(25),
            )?;
            Ok((signature(&output), output.trim().to_string()))
        }
        other => Err(format!("fonte desconhecida: {other}")),
    }
}

fn run_due_watchers(push: &Arc<dyn Fn(Trigger) + Send + Sync>) {
    let path = data_dir().join("watchers.json");
    let data = read_json(path.clone(), json!([]));
    let Some(watchers) = data.as_array() else {
        return;
    };
    let now = now_secs();
    let mut updated: Vec<Value> = Vec::new();
    let mut changed = false;

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

        let source = watcher.get("source").and_then(|value| value.as_str()).unwrap_or("");
        let params = watcher.get("params").cloned().unwrap_or(Value::Null);
        let name = watcher.get("name").and_then(|value| value.as_str()).unwrap_or("watcher").to_string();

        match read_source(source, &params) {
            Ok((sig, detail)) => {
                let previous = watcher.get("last_value").and_then(|value| value.as_str()).unwrap_or("");
                let first = previous.is_empty();
                let fire_on_start = watcher
                    .get("fire_on_start")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let contains_ok = params
                    .get("contains")
                    .and_then(|value| value.as_str())
                    .map(|needle| detail.to_lowercase().contains(&needle.to_lowercase()))
                    .unwrap_or(true);

                if (!first && previous != sig && contains_ok) || (first && fire_on_start) {
                    let prompt = watcher.get("prompt").and_then(|value| value.as_str()).unwrap_or("");
                    push(Trigger {
                        source: format!("watcher:{name}"),
                        prompt: format!(
                            "{prompt}\n\nMudança detectada em `{name}` ({source}):\n{}",
                            detail.chars().take(1500).collect::<String>()
                        ),
                    });
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
        write_json(path, &Value::Array(updated));
    }
}
