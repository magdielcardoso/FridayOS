//! Action journal, `undo_last` and `action_log`.
//!
//! [`Registry::call`](crate::registry::Registry::call) snapshots a compact
//! backup *before* a mutating tool runs and appends a record only if the call
//! succeeded. `undo_last` reverses what is safe to reverse (write/move/delete)
//! and reports a per-item status for everything else. The journal is capped at
//! 500 lines and rotates to `actions.1.jsonl`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

const JOURNAL_CAP: usize = 500;
const MAX_BACKUP_BYTES: u64 = 1024 * 1024;
const MAX_ARGS_CHARS: usize = 200;
const MAX_UNDO: u64 = 10;
const DEFAULT_LOG_LIMIT: u64 = 50;

// ---------------------------------------------------------------------------
// Journal plumbing (called by Registry::call)
// ---------------------------------------------------------------------------

fn journal_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/FridayOS")
        .join("actions.jsonl")
}

/// Tools whose successful calls are worth journaling. Includes the conceptual
/// names from the autonomy spec (`fs_delete`, `fs_rename`, `app_open`) as well
/// as the concrete names this crate registers (`fs_trash`, `fs_move`,
/// `app_launch`), so the journal stays correct if aliases are added later.
pub(crate) fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "fs_write"
            | "fs_delete"
            | "fs_move"
            | "fs_rename"
            | "fs_mkdir"
            | "fs_trash"
            | "app_open"
            | "app_launch"
            | "app_quit"
    )
}

/// Resolves a user path the same way the filesystem tools do: expand `~`,
/// anchor relative paths at the session cwd, reject NUL and `..`. Returns
/// `None` for anything unsafe — in that case no backup is captured.
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

fn text_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Captures a small backup of what the call is about to change. Read-only: it
/// never creates or modifies files. Called before `Tool::call`.
pub(crate) fn capture_backup(name: &str, args: &Value, ctx: &ToolCtx) -> Value {
    match name {
        "fs_write" => {
            let path = arg_str(args, "path").and_then(|raw| resolve_path(raw, ctx));
            let (path_value, content_b64) = match path {
                Some(path) => (json!(text_of(&path)), readable_b64(&path)),
                None => (Value::Null, None),
            };
            json!({ "kind": "content", "path": path_value, "content_b64": content_b64 })
        }
        "fs_move" | "fs_rename" => {
            let from = arg_str(args, "from")
                .and_then(|raw| resolve_path(raw, ctx))
                .map(|path| json!(text_of(&path)))
                .unwrap_or(Value::Null);
            let to = arg_str(args, "to")
                .and_then(|raw| resolve_path(raw, ctx))
                .map(|path| json!(text_of(&path)))
                .unwrap_or(Value::Null);
            json!({ "kind": "move", "from": from, "to": to })
        }
        "fs_delete" | "fs_trash" => {
            let path = arg_str(args, "path").and_then(|raw| resolve_path(raw, ctx));
            let (path_value, content_b64) = match path {
                Some(path) => (json!(text_of(&path)), readable_b64(&path)),
                None => (Value::Null, None),
            };
            json!({ "kind": "delete", "path": path_value, "content_b64": content_b64 })
        }
        _ => json!({ "kind": "none" }),
    }
}

/// Base64 of a small regular file, or `None` when missing, not a file, too big
/// or unreadable. Keeps backups bounded (< 1 MB before encoding).
fn readable_b64(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() >= MAX_BACKUP_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Appends one compact record after a successful mutating call. Best-effort:
/// a failure to write the journal must never fail the original action.
pub(crate) fn record_action(name: &str, args: &Value, backup: Value, _ctx: &ToolCtx) {
    let path = journal_path();
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }

    let entry = json!({
        "ts": now_secs(),
        "tool": name,
        "args": redact_args(args),
        "backup": backup,
    });

    if count_lines(&path) >= JOURNAL_CAP {
        let rotated = path.with_file_name("actions.1.jsonl");
        let _ = fs::remove_file(&rotated);
        if fs::rename(&path, &rotated).is_err() {
            return;
        }
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{entry}");
    }
}

fn count_lines(path: &Path) -> usize {
    match fs::read(path) {
        Ok(bytes) => bytes.iter().filter(|&&byte| byte == b'\n').count(),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Redaction (never persist secrets or long content in the journal)
// ---------------------------------------------------------------------------

fn is_secret_key(key: &str) -> bool {
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
    ]
    .iter()
    .any(|needle| lowered.contains(*needle))
}

fn truncate_string(value: &str) -> Value {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= MAX_ARGS_CHARS {
            out.push('…');
            return Value::String(out);
        }
        out.push(ch);
    }
    Value::String(out)
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => truncate_string(text),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(redact_value)
                .collect::<Vec<_>>(),
        ),
        Value::Object(map) => {
            let mut redacted = serde_json::Map::new();
            for (key, child) in map.iter().take(20) {
                let value = if is_secret_key(key) {
                    Value::String("<redacted>".to_string())
                } else {
                    redact_value(child)
                };
                redacted.insert(key.clone(), value);
            }
            Value::Object(redacted)
        }
        other => other.clone(),
    }
}

fn redact_args(args: &Value) -> Value {
    match args {
        Value::Object(map) => {
            let mut redacted = serde_json::Map::new();
            for (key, child) in map {
                let value = if is_secret_key(key) {
                    Value::String("<redacted>".to_string())
                } else {
                    redact_value(child)
                };
                redacted.insert(key.clone(), value);
            }
            Value::Object(redacted)
        }
        other => redact_value(other),
    }
}

// ---------------------------------------------------------------------------
// Tool plumbing
// ---------------------------------------------------------------------------

struct UndoTool {
    spec: ToolSpec,
}

impl Tool for UndoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "undo_last" => undo_last(args, ctx),
            "action_log" => action_log(args),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(UndoTool {
        spec: ToolSpec {
            name: "undo_last",
            summary: "Desfaz as últimas ações registradas no diário (padrão: 1, máximo: 10): restaura conteúdo, move de volta ou recria arquivos. Muda arquivos e precisa de confirmação.",
            risk: Risk::Sensitive,
            args: json!({
                "type": "object",
                "properties": {
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "default": 1,
                        "description": "Quantas ações recentes desfazer."
                    }
                },
                "additionalProperties": false
            }),
        },
    });
    registry.add(UndoTool {
        spec: ToolSpec {
            name: "action_log",
            summary: "Consulta o diário de ações da Friday, com limite e filtro opcional por ferramenta.",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 500,
                        "default": 50,
                        "description": "Número máximo de registros (mais recentes primeiro)."
                    },
                    "tool": {
                        "type": "string",
                        "description": "Filtra por nome da ferramenta, ex.: \"fs_write\"."
                    }
                },
                "additionalProperties": false
            }),
        },
    });
}

// ---------------------------------------------------------------------------
// undo_last
// ---------------------------------------------------------------------------

fn read_records() -> Result<Vec<Value>, ToolError> {
    let path = journal_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ToolError::failed(format!(
                "não consegui ler o diário de ações: {error}"
            )))
        }
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect())
}

fn undo_action(record: &Value) -> String {
    let tool = record.get("tool").and_then(Value::as_str).unwrap_or("?");
    match tool {
        "fs_write" => "restaurar o conteúdo anterior do arquivo".to_string(),
        "fs_move" | "fs_rename" => "mover o arquivo de volta à origem".to_string(),
        "fs_delete" | "fs_trash" => "recriar o arquivo a partir do backup".to_string(),
        other => format!("sem reversão automática para `{other}`"),
    }
}

fn undo_last(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let count = match args.get("count") {
        None | Some(Value::Null) => 1,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| ToolError::invalid("`count` deve ser um inteiro positivo"))?
            .clamp(1, MAX_UNDO),
        Some(_) => return Err(ToolError::invalid("`count` deve ser um inteiro positivo")),
    } as usize;

    let records = read_records()?;
    let selected: Vec<Value> = records.into_iter().rev().take(count).collect();
    if selected.is_empty() {
        return Ok(json!({
            "ok": true,
            "undone": 0,
            "results": [],
            "message": "diário de ações vazio; nada a desfazer",
        }));
    }

    let preview: Vec<Value> = selected
        .iter()
        .map(|record| {
            json!({
                "tool": record.get("tool").cloned().unwrap_or(Value::Null),
                "action": undo_action(record),
            })
        })
        .collect();

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_undo": preview,
        }));
    }

    // Undo overwrites/restores real files. Even when the autonomy policy allows
    // it (e.g. "full"), never mutate user data here without an explicit
    // confirmation from the voice/UI layer.
    if !ctx.confirmed {
        return Ok(json!({
            "ok": false,
            "needs_confirmation": true,
            "count": selected.len(),
            "message": "desfazer altera arquivos; confirme para executar",
            "preview": preview,
        }));
    }

    let mut results = Vec::new();
    let mut undone = 0usize;
    for record in &selected {
        let status = undo_one(record);
        if status.get("status").and_then(Value::as_str) == Some("undone") {
            undone += 1;
        }
        results.push(status);
    }

    Ok(json!({
        "ok": true,
        "undone": undone,
        "attempted": selected.len(),
        "results": results,
    }))
}

fn status(tool: &str, state: &str, detail: impl Into<String>) -> Value {
    json!({ "tool": tool, "status": state, "detail": detail.into() })
}

fn undo_one(record: &Value) -> Value {
    let tool = record.get("tool").and_then(Value::as_str).unwrap_or("");
    let backup = record.get("backup").cloned().unwrap_or(Value::Null);

    match tool {
        "fs_write" => {
            let (Some(path_text), Some(encoded)) = (
                backup.get("path").and_then(Value::as_str),
                backup.get("content_b64").and_then(Value::as_str),
            ) else {
                return status(tool, "skipped", "sem backup de conteúdo para restaurar");
            };
            let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) => bytes,
                Err(error) => return status(tool, "failed", format!("backup inválido: {error}")),
            };
            let path = PathBuf::from(path_text);
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&path, bytes) {
                Ok(_) => status(tool, "undone", "conteúdo anterior restaurado"),
                Err(error) => status(tool, "failed", format!("{error}")),
            }
        }
        "fs_move" | "fs_rename" => {
            let (Some(from_text), Some(to_text)) = (
                backup.get("from").and_then(Value::as_str),
                backup.get("to").and_then(Value::as_str),
            ) else {
                return status(tool, "skipped", "origem/destino ausentes no backup");
            };
            let from = PathBuf::from(from_text);
            let to = PathBuf::from(to_text);
            if !to.exists() {
                return status(tool, "skipped", "arquivo movido não existe mais");
            }
            if from.exists() {
                return status(
                    tool,
                    "skipped",
                    "destino da restauração já existe; não sobrescrevendo",
                );
            }
            if let Some(parent) = from.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::rename(&to, &from) {
                Ok(_) => status(tool, "undone", "arquivo movido de volta à origem"),
                Err(error) => status(tool, "failed", format!("{error}")),
            }
        }
        "fs_delete" | "fs_trash" => {
            let Some(path_text) = backup.get("path").and_then(Value::as_str) else {
                return status(tool, "skipped", "caminho original ausente no backup");
            };
            let path = PathBuf::from(path_text);
            if path.exists() {
                return status(tool, "skipped", "arquivo já existe no caminho original");
            }
            let bytes = backup
                .get("content_b64")
                .and_then(Value::as_str)
                .and_then(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded).ok());
            let Some(bytes) = bytes else {
                return status(
                    tool,
                    "skipped",
                    "sem conteúdo em backup para recriar o arquivo",
                );
            };
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&path, bytes) {
                Ok(_) => status(tool, "undone", "arquivo recriado a partir do backup"),
                Err(error) => status(tool, "failed", format!("{error}")),
            }
        }
        other => status(other, "skipped", format!("sem reversão automática para `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// action_log
// ---------------------------------------------------------------------------

fn action_log(args: &Value) -> ToolResult {
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LOG_LIMIT,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| ToolError::invalid("`limit` deve ser um inteiro positivo"))?
            .clamp(1, JOURNAL_CAP as u64),
        Some(_) => return Err(ToolError::invalid("`limit` deve ser um inteiro positivo")),
    } as usize;

    let tool_filter = args
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let records = read_records()?;
    let filtered: Vec<&Value> = records
        .iter()
        .filter(|record| match tool_filter {
            Some(wanted) => record.get("tool").and_then(Value::as_str) == Some(wanted),
            None => true,
        })
        .collect();
    let total = filtered.len();
    let entries: Vec<&Value> = filtered.into_iter().rev().take(limit).collect();

    Ok(json!({
        "ok": true,
        "count": entries.len(),
        "total": total,
        "entries": entries,
    }))
}
