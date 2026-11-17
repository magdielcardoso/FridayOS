use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, PathBuf};

use serde_json::Value;

use crate::audit::AuditLog;
use crate::policy::{self, Risk};

pub type ToolResult = Result<Value, ToolError>;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("ação negada: {0}")]
    Denied(String),
    #[error("argumentos inválidos: {0}")]
    InvalidArgs(String),
    #[error("ferramenta falhou: {0}")]
    Failed(String),
    #[error("ferramenta não encontrada: {0}")]
    NotFound(String),
}

impl ToolError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgs(message.into())
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub risk: Risk,
    /// JSON Schema for the arguments, shown to the model / MCP client.
    pub args: Value,
}

pub struct ToolCtx {
    /// The voice/UI layer already confirmed this specific action.
    pub confirmed: bool,
    /// Do not perform side effects; report what would happen.
    pub dry_run: bool,
    pub audit: AuditLog,
    pub cwd: PathBuf,
}

impl ToolCtx {
    pub fn new(audit: AuditLog) -> Self {
        Self {
            confirmed: false,
            dry_run: false,
            audit,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn confirmed(mut self) -> Self {
        self.confirmed = true;
        self
    }

    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    /// Performs the action. Implementations must check `ctx` and must not
    /// touch the system when `ctx.dry_run` is set.
    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult;

    /// Called by the registry before `call`. Default: autonomy policy.
    fn authorize(&self, ctx: &ToolCtx) -> ToolResult {
        policy::authorize_tool(self.spec().name, self.spec().risk, ctx)
    }
}

#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<&'static str, Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        crate::tools::register_all(&mut registry);
        registry
    }

    pub fn add(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.spec().name, Box::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|tool| tool.as_ref())
    }

    pub fn specs(&self) -> Vec<&ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    pub fn call(&self, name: &str, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?;
        tool.authorize(ctx)?;
        ctx.audit.record(name, tool.spec().risk, args, ctx.dry_run);

        // Snapshot whatever the action is about to change *before* it runs,
        // then journal it only if the call actually succeeded. Dry runs perform
        // no action, so they never enter the action journal.
        let journaling = !ctx.dry_run && crate::tools::undo::is_mutating_tool(name);
        let backup = journaling
            .then(|| crate::tools::undo::capture_backup(name, args, ctx));

        let result = tool.call(args, ctx);

        if journaling && result.is_ok() {
            if let Some(backup) = backup {
                crate::tools::undo::record_action(name, args, backup, ctx);
            }
        }

        // Governance/verification layer: best-effort, never influences the
        // original result. Only successful, mutating, non-dry-run calls are
        // recorded; see the `verify` module below for the checks.
        if !ctx.dry_run && tool.spec().risk != Risk::ReadOnly && verify::is_mutating(name) {
            if let Ok(value) = result.as_ref() {
                verify::record(name, args, value, ctx);
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Verification / governance layer
//
// Every successful mutating call leaves a compact, local record in
// `~/Library/Application Support/FridayOS/verification.jsonl`. The layer is
// strictly best-effort: it never blocks the action, never touches the network,
// never panics and never writes secrets or token values to disk. Deterministic
// checks (result flags, filesystem state) run first; process probes are only
// attempted when they can complete within a tiny budget, otherwise the check is
// reported as `unknown`.
// ---------------------------------------------------------------------------

const VERIFICATION_CAP: usize = 500;

fn verification_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/FridayOS")
        .join("verification.jsonl")
}

/// Returns the most recent verification records (newest first), bounded to the
/// last [`VERIFICATION_CAP`] lines of the journal. Unreadable or malformed
/// lines are skipped; this function never returns an error or panics.
pub fn recent_verifications(limit: usize) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(verification_path()) else {
        return Vec::new();
    };
    let cap = limit.min(VERIFICATION_CAP);
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .take(cap)
        .collect()
}

mod verify {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Read as _, Write as _};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::json;

    const MAX_ARG_CHARS: usize = 120;
    /// Hard ceiling for the optional `ps` probe. Kept well below the ~50 ms
    /// budget of the whole layer; on timeout the check is simply `unknown`.
    const PROCESS_BUDGET: Duration = Duration::from_millis(30);

    fn secret_key(key: &str) -> bool {
        let lowered = key.to_lowercase();
        [
            "password",
            "passwd",
            "secret",
            "token",
            "api_key",
            "apikey",
            "credential",
            "authorization",
            "auth",
            "private_key",
            "keychain",
            "session",
            "cookie",
            "bearer",
        ]
        .iter()
        .any(|needle| lowered.contains(*needle))
    }

    fn redact(value: &Value) -> Value {
        match value {
            Value::String(text) => {
                let mut out = String::new();
                for (index, ch) in text.chars().enumerate() {
                    if index >= MAX_ARG_CHARS {
                        out.push('…');
                        break;
                    }
                    out.push(ch);
                }
                Value::String(out)
            }
            Value::Array(items) => Value::Array(items.iter().take(20).map(redact).collect()),
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (key, child) in map.iter().take(20) {
                    let safe = if secret_key(key) {
                        Value::String("<redacted>".to_string())
                    } else {
                        redact(child)
                    };
                    out.insert(key.clone(), safe);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    /// Stable, short digest of the canonical result JSON (FNV-1a, no deps).
    fn digest(value: &Value) -> String {
        let text = serde_json::to_string(value).unwrap_or_default();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in text.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Tools whose successful calls are worth verifying. Prefixes cover whole
    /// families; the explicit list pins down named mutations that do not share
    /// one. Read-only tools are filtered out by the caller via `Risk`.
    pub(super) fn is_mutating(name: &str) -> bool {
        const PREFIXES: &[&str] = &[
            "fs_",
            "app_",
            "process_",
            "power_",
            "keychain_",
            "wifi_",
            "shell_",
            "applescript_",
            "jxa_",
            "autonomy_",
            "reminder_",
            "watcher_",
            "webhook_",
            "routine_",
        ];
        const EXACT: &[&str] = &[
            "codex_login",
            "codex_account_switch",
            "calendar_create",
            "calendar_update",
            "calendar_delete",
            "mission_update",
            "mission_complete",
            "mission_cancel",
            "assistant_task",
            "assistant_cancel",
        ];
        PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || EXACT.contains(&name)
    }

    /// Mirrors the filesystem tools' path resolution: expand `~`, anchor
    /// relative paths at the session cwd, reject NUL and `..`.
    fn resolve_path(raw: &str, ctx: &ToolCtx) -> Option<PathBuf> {
        if raw.trim().is_empty() || raw.contains('\0') {
            return None;
        }
        let expanded = if raw == "~" {
            PathBuf::from(std::env::var_os("HOME")?)
        } else if let Some(rest) = raw.strip_prefix("~/") {
            PathBuf::from(std::env::var_os("HOME")?).join(rest)
        } else {
            PathBuf::from(raw)
        };
        if expanded
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return None;
        }
        Some(if expanded.is_absolute() {
            expanded
        } else {
            ctx.cwd.join(expanded)
        })
    }

    fn result_failure(value: &Value) -> Option<String> {
        let object = value.as_object()?;
        if object.get("ok").and_then(Value::as_bool) == Some(false) {
            return Some("result.ok=false".to_string());
        }
        if let Some(error) = object.get("error") {
            let meaningful = match error {
                Value::Null => false,
                Value::String(text) => !text.trim().is_empty(),
                _ => true,
            };
            if meaningful {
                return Some("result.error".to_string());
            }
        }
        None
    }

    /// Returns `(arg_key, expect_present)` when a path check applies. The
    /// explicit `action` argument wins over an inference from the tool name.
    fn path_expectation(name: &str, args: &Value) -> Option<(&'static str, bool)> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_lowercase);
        let signal = action.as_deref().unwrap_or(name).to_lowercase();
        let expect_present = if ["delete", "trash", "remove", "erase", "unlink"]
            .iter()
            .any(|word| signal.contains(word))
        {
            false
        } else if ["write", "create", "mkdir", "save", "set", "add"]
            .iter()
            .any(|word| signal.contains(word))
        {
            true
        } else {
            return None;
        };
        for key in ["path", "to", "from"] {
            if args.get(key).and_then(Value::as_str).is_some() {
                return Some((key, expect_present));
            }
        }
        None
    }

    /// Best-effort process probe. `ps` is spawned with fixed argv (no shell),
    /// polled only for a few milliseconds and killed on timeout. `None` means
    /// "unknown" and never turns into a failure.
    fn process_present(name: &str) -> Option<bool> {
        if name.is_empty() || name.contains('\0') {
            return None;
        }
        let mut child = Command::new("ps")
            .arg("-axo")
            .arg("comm=")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + PROCESS_BUDGET;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let mut stdout = child.stdout.take()?;
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let needle = name.to_lowercase();
        Some(out.lines().any(|line| line.to_lowercase().contains(&needle)))
    }

    /// Derives a short process name for `app_launch` / `app_quit`. Bundle ids
    /// and other dotted identifiers are rejected so the probe stays `unknown`
    /// rather than producing a false failure.
    fn app_target(args: &Value, result: &Value) -> Option<String> {
        let raw = ["name", "app", "path_or_bundle", "target", "bundle", "bundle_id"]
            .iter()
            .find_map(|key| args.get(*key).and_then(Value::as_str))
            .or_else(|| result.get("launched").and_then(Value::as_str))?;
        let mut clean = raw.trim();
        if let Some(last) = clean.rsplit('/').next() {
            clean = last;
        }
        if let Some(stem) = clean.strip_suffix(".app") {
            clean = stem;
        }
        if clean.is_empty() || clean.contains('.') {
            return None;
        }
        Some(clean.to_lowercase())
    }

    fn check(name: &str, status: &str, detail: &str) -> Value {
        json!({ "name": name, "status": status, "detail": detail })
    }

    fn append(entry: &Value) {
        let path = verification_path();
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let lines = fs::read(&path)
            .map(|bytes| bytes.iter().filter(|&&byte| byte == b'\n').count())
            .unwrap_or(0);
        if lines >= super::VERIFICATION_CAP {
            let rotated = path.with_file_name("verification.1.jsonl");
            let _ = fs::remove_file(&rotated);
            if fs::rename(&path, &rotated).is_err() {
                return;
            }
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "{entry}");
        }
    }

    /// Appends one verification record after a successful mutating call.
    pub(super) fn record(name: &str, args: &Value, result: &Value, ctx: &ToolCtx) {
        let mut checks: Vec<Value> = Vec::new();
        let mut status = "pass";

        // 1. Deterministic: the tool reported failure inside its JSON result.
        match result_failure(result) {
            Some(detail) => {
                checks.push(check("result", "fail", &detail));
                status = "fail";
            }
            None => checks.push(check("result", "pass", "ok")),
        }

        // 2. Deterministic: filesystem state matches a write/create/delete.
        if let Some((key, expect_present)) = path_expectation(name, args) {
            let resolved = args
                .get(key)
                .and_then(Value::as_str)
                .and_then(|raw| resolve_path(raw, ctx));
            match resolved {
                Some(path) => {
                    let exists = path.exists();
                    let ok = exists == expect_present;
                    let label = if expect_present {
                        "path_exists"
                    } else {
                        "path_absent"
                    };
                    checks.push(check(
                        label,
                        if ok { "pass" } else { "fail" },
                        &path.to_string_lossy(),
                    ));
                    if !ok {
                        status = "fail";
                    }
                }
                None => {
                    checks.push(check("path", "unknown", "unresolved"));
                    if status != "fail" {
                        status = "unknown";
                    }
                }
            }
        }

        // 3. Cheap-only: process presence after app launch/quit.
        if name == "app_launch" || name == "app_quit" {
            let expect_present = name == "app_launch";
            match app_target(args, result).and_then(|target| process_present(&target)) {
                Some(present) => {
                    let ok = present == expect_present;
                    let label = if expect_present {
                        "process_present"
                    } else {
                        "process_absent"
                    };
                    checks.push(check(label, if ok { "pass" } else { "fail" }, "ps"));
                    if !ok {
                        status = "fail";
                    }
                }
                None => {
                    checks.push(check("process", "unknown", "ps-skipped"));
                    if status != "fail" {
                        status = "unknown";
                    }
                }
            }
        }

        let entry = json!({
            "ts": now_secs(),
            "tool": name,
            "args_redacted": redact(args),
            "result_digest": digest(result),
            "status": status,
            "checks": checks,
        });
        append(&entry);
    }
}

/// Runs a macOS CLI and returns stdout, mapping failures to `ToolError`.
pub(crate) fn run(cmd: &str, args: &[&str]) -> ToolResult {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| ToolError::Failed(format!("{cmd}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Failed(format!(
            "{cmd} saiu com {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(Value::String(String::from_utf8_lossy(&output.stdout).to_string()))
}
