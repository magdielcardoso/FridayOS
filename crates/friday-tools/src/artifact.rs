//! Bounded, on-disk state/artifact store for orchestrator–worker runs.
//!
//! Every run gets its own directory under
//! `~/Library/Application Support/FridayOS/runs/<stamp>-<slug>-<hex>/`:
//!
//! - `plan.md` — the goal/plan the worker is executing
//! - `findings.jsonl` — append-only observations, capped at 500 lines
//! - `tool-results/` — capped at 200 JSON files, with secrets redacted
//! - `verification.json` — latest verification status + note
//!
//! The helpers are tolerant of missing/corrupt files and never panic: a broken
//! artifact degrades to an empty read, never to a crash. Writes use a temp file
//! plus rename so a partially written record is never observed.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const MAX_FINDINGS: usize = 500;
const MAX_TOOL_RESULTS: usize = 200;

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

/// Root of all run directories. Public so read-only tools can enumerate runs.
pub fn runs_dir() -> PathBuf {
    data_dir().join("runs")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Compact UTC timestamp `YYYYMMDD-HHMMSS`, computed without a date crate.
fn stamp() -> String {
    let secs = now_secs() as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = sod / 3600;
    let minute = (sod % 3600) / 60;
    let second = sod % 60;
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 -> (year, month, day).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let mut year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = ((mp as i32) + if mp < 10 { 3 } else { -9 }) as u32; // [1, 12]
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn slug(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "run".into()
    } else {
        trimmed.chars().take(32).collect()
    }
}

/// Short, unique-enough suffix derived from the clock and process id.
fn short_hex() -> String {
    let mixed = (now_nanos() as u64)
        ^ (std::process::id() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!("{:08x}", mixed as u32)
}

/// Atomic-ish write: unique temp file + rename, with cleanup on failure.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("record");
    let tmp = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        name,
        std::process::id(),
        now_nanos() % 1_000_000_000
    ));
    fs::write(&tmp, bytes).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    Ok(())
}

/// Recursively replace values stored under sensitive-looking keys.
fn redact(value: &Value) -> Value {
    const SENSITIVE: [&str; 4] = ["token", "password", "secret", "authorization"];
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                let sensitive = SENSITIVE.iter().any(|needle| lower.contains(needle));
                if sensitive {
                    out.insert(key.clone(), json!("[REDACTED]"));
                } else {
                    out.insert(key.clone(), redact(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// A handle to one run directory. Clone by rebuilding from [`load_run`] if needed.
#[derive(Debug, Clone)]
pub struct RunArtifact {
    pub id: String,
    pub dir: PathBuf,
}

impl RunArtifact {
    fn findings_path(&self) -> PathBuf {
        self.dir.join("findings.jsonl")
    }

    fn verification_path(&self) -> PathBuf {
        self.dir.join("verification.json")
    }

    fn tools_dir(&self) -> PathBuf {
        self.dir.join("tool-results")
    }

    /// Append `{ts,text}` to `findings.jsonl`, keeping at most 500 lines.
    pub fn append_finding(&self, text: &str) -> Result<(), String> {
        let path = self.findings_path();
        let mut lines: VecDeque<String> = VecDeque::new();
        if let Ok(raw) = fs::read_to_string(&path) {
            for line in raw.lines().rev().take(MAX_FINDINGS.saturating_sub(1)) {
                if !line.trim().is_empty() {
                    lines.push_front(line.to_string());
                }
            }
        }
        let record = json!({ "ts": now_secs(), "text": text });
        let encoded = serde_json::to_string(&record).map_err(|error| error.to_string())?;
        lines.push_back(encoded);
        let mut output = String::new();
        for line in &lines {
            output.push_str(line);
            output.push('\n');
        }
        atomic_write(&path, output.as_bytes())
    }

    /// Store a redacted tool result as `tool-results/<seq>-<tool>.json`,
    /// retaining at most 200 files (oldest sequence numbers dropped first).
    pub fn record_tool(&self, tool: &str, result: &Value) -> Result<(), String> {
        let tools = self.tools_dir();
        fs::create_dir_all(&tools).map_err(|error| error.to_string())?;

        let counter_path = tools.join(".seq");
        let seq = fs::read_to_string(&counter_path)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .wrapping_add(1);
        let _ = fs::write(&counter_path, seq.to_string());

        let bytes = serde_json::to_vec_pretty(&redact(result))
            .map_err(|error| error.to_string())?;
        let file = tools.join(format!("{seq:08}-{name}.json", name = slug(tool)));
        atomic_write(&file, &bytes)?;

        let mut files: Vec<PathBuf> = fs::read_dir(&tools)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json"))
                    .then_some(path)
            })
            .collect();
        let excess = files.len().saturating_sub(MAX_TOOL_RESULTS);
        if excess > 0 {
            files.sort();
            for path in files.into_iter().take(excess) {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }

    /// Write `verification.json` with `{status, note, updated}`.
    pub fn set_verification(&self, status: &str, note: &str) -> Result<(), String> {
        let status = if status.trim().is_empty() {
            "unknown"
        } else {
            status.trim()
        };
        let record = json!({
            "status": status,
            "note": note,
            "updated": now_secs(),
        });
        let bytes = serde_json::to_vec_pretty(&record).map_err(|error| error.to_string())?;
        atomic_write(&self.verification_path(), &bytes)
    }
}

/// Start a fresh run: create the directory skeleton and an initial plan whose
/// goal is `label`. A colliding name gets a numeric suffix instead of failing.
pub fn start_run(label: &str) -> Result<RunArtifact, String> {
    let label = label.trim();
    let stamp = stamp();
    let base = format!("{}-{}-{}", stamp, slug(label), short_hex());
    let root = runs_dir();
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;

    let mut id = base.clone();
    let mut dir = root.join(&id);
    let mut counter = 0u32;
    while dir.exists() {
        counter = counter.saturating_add(1);
        id = format!("{base}-{counter}");
        dir = root.join(&id);
        if counter >= 9999 {
            return Err("não foi possível criar um id de execução único".into());
        }
    }

    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    fs::create_dir_all(dir.join("tool-results")).map_err(|error| error.to_string())?;

    let plan = format!(
        "# Plano: {label}\n\n- Objetivo: {label}\n- ID: {id}\n- Criado (UTC): {stamp}\n\n\
         ## Etapas\n\n1. Coletar evidências com ferramentas auditadas.\n\
         2. Registrar cada descoberta em `findings.jsonl`.\n\
         3. Verificar o resultado e atualizar `verification.json`.\n"
    );
    atomic_write(&dir.join("plan.md"), plan.as_bytes())?;
    atomic_write(&dir.join("findings.jsonl"), b"")?;
    let initial = serde_json::to_vec_pretty(&json!({
        "status": "unknown",
        "note": "",
        "updated": now_secs(),
    }))
    .map_err(|error| error.to_string())?;
    atomic_write(&dir.join("verification.json"), &initial)?;

    Ok(RunArtifact { id, dir })
}

/// Reject ids that could escape the runs directory.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
}

/// Reopen a run by id, or `None` when it is missing/invalid.
pub fn load_run(id: &str) -> Option<RunArtifact> {
    if !safe_id(id) {
        return None;
    }
    let dir = runs_dir().join(id);
    dir.is_dir().then(|| RunArtifact { id: id.to_string(), dir })
}

/// Latest verification `(status, note)` for a run, when available.
pub fn verification_status(id: &str) -> Option<(String, String)> {
    let run = load_run(id)?;
    let raw = fs::read_to_string(run.verification_path()).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let note = value
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((status, note))
}

/// Newest-first, bounded list of runs with their verification state.
pub fn list_runs(limit: usize) -> Vec<Value> {
    if limit == 0 {
        return Vec::new();
    }
    let mut runs: Vec<Value> = Vec::new();
    if let Ok(entries) = fs::read_dir(runs_dir()) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if id.is_empty() || !safe_id(&id) {
                continue;
            }
            let verification = fs::read_to_string(dir.join("verification.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({ "status": "unknown", "note": "" }));
            let findings = fs::read_to_string(dir.join("findings.jsonl"))
                .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
                .unwrap_or(0);
            runs.push(json!({
                "id": id,
                "run_id": id,
                "created": id.chars().take(15).collect::<String>(),
                "verification": verification.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                "note": verification.get("note").and_then(Value::as_str).unwrap_or(""),
                "findings": findings,
            }));
        }
    }
    runs.sort_by(|a, b| {
        let a_id = a.get("id").and_then(Value::as_str).unwrap_or("");
        let b_id = b.get("id").and_then(Value::as_str).unwrap_or("");
        b_id.cmp(a_id)
    });
    runs.truncate(limit);
    runs
}
