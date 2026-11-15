//! System watchers: observe email, iMessage, files, URLs, clipboard or any
//! shell command and wake Friday when the observed value changes.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/FridayOS")
        .join("watchers.json")
}

fn read_list() -> Vec<Value> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn write_list(items: &[Value]) -> Result<(), ToolError> {
    if let Some(parent) = path().parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::failed(e.to_string()))?;
    }
    let text = serde_json::to_vec_pretty(&Value::Array(items.to_vec()))
        .map_err(|e| ToolError::failed(e.to_string()))?;
    let tmp = path().with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| ToolError::failed(e.to_string()))?;
    std::fs::rename(tmp, path()).map_err(|e| ToolError::failed(e.to_string()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

struct WatcherTool {
    spec: ToolSpec,
}

impl Tool for WatcherTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "watcher_create" => {
                let name = arg(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let source = arg(args, "source").ok_or_else(|| ToolError::invalid("informe `source`"))?;
                let prompt = arg(args, "prompt").ok_or_else(|| ToolError::invalid("informe `prompt`"))?;
                let mut params = serde_json::Map::new();
                for key in ["command", "path", "url", "contains"] {
                    if let Some(value) = arg(args, key) {
                        params.insert(key.to_string(), json!(value));
                    }
                }
                let required = match source {
                    "shell" => "command",
                    "file" => "path",
                    "http" => "url",
                    "clipboard" | "imessage" | "email" => "",
                    other => {
                        return Err(ToolError::invalid(format!(
                            "fonte inválida `{other}`: use shell, file, http, clipboard, imessage ou email"
                        )))
                    }
                };
                if !required.is_empty() && !params.contains_key(required) {
                    return Err(ToolError::invalid(format!("fonte `{source}` exige `{required}`")));
                }
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_watch": name, "source": source }));
                }
                let mut list = read_list();
                list.retain(|watcher| watcher.get("name").and_then(|v| v.as_str()) != Some(name));
                list.push(json!({
                    "name": name,
                    "source": source,
                    "prompt": prompt,
                    "params": params,
                    "interval_secs": args.get("interval_secs").and_then(|v| v.as_u64()).unwrap_or(60).max(15),
                    "fire_on_start": args.get("fire_on_start").and_then(|v| v.as_bool()).unwrap_or(false),
                    "enabled": true,
                    "last_check": 0,
                    "last_value": "",
                    "created": now(),
                }));
                write_list(&list)?;
                Ok(json!({
                    "ok": true,
                    "name": name,
                    "source": source,
                    "note": "o primeiro ciclo define a linha de base; mudanças depois disparam o prompt"
                }))
            }
            "watcher_list" => Ok(json!({ "ok": true, "watchers": read_list() })),
            "watcher_delete" => {
                let name = arg(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_delete": name }));
                }
                let mut list = read_list();
                let before = list.len();
                list.retain(|watcher| watcher.get("name").and_then(|v| v.as_str()) != Some(name));
                write_list(&list)?;
                Ok(json!({ "ok": true, "removed": before - list.len() }))
            }
            "watcher_check" => {
                let name = arg(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_check": name }));
                }
                let mut list = read_list();
                if let Some(watcher) = list
                    .iter_mut()
                    .find(|watcher| watcher.get("name").and_then(|v| v.as_str()) == Some(name))
                {
                    watcher["last_check"] = json!(0);
                    write_list(&list)?;
                    Ok(json!({ "ok": true, "queued": name, "note": "o ciclo roda em até 5 s" }))
                } else {
                    Err(ToolError::failed(format!("watcher `{name}` não existe")))
                }
            }
            "watcher_pause" => {
                let name = arg(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_set": enabled }));
                }
                let mut list = read_list();
                if let Some(watcher) = list
                    .iter_mut()
                    .find(|watcher| watcher.get("name").and_then(|v| v.as_str()) == Some(name))
                {
                    watcher["enabled"] = json!(enabled);
                    write_list(&list)?;
                    Ok(json!({ "ok": true, "enabled": enabled }))
                } else {
                    Err(ToolError::failed(format!("watcher `{name}` não existe")))
                }
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> WatcherTool {
    WatcherTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "watcher_create",
        "Observa uma fonte do sistema (shell, arquivo, URL, clipboard, iMessage, e-mail) e acorda a Friday quando o valor mudar.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name", "source", "prompt"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "source": { "type": "string", "enum": ["shell", "file", "http", "clipboard", "imessage", "email"] },
                "prompt": { "type": "string", "description": "O que fazer quando mudar." },
                "command": { "type": "string", "description": "Para source=shell." },
                "path": { "type": "string", "description": "Para source=file." },
                "url": { "type": "string", "description": "Para source=http." },
                "contains": { "type": "string", "description": "Só dispara se o detalhe contiver este texto." },
                "interval_secs": { "type": "integer", "minimum": 15, "default": 60 },
                "fire_on_start": { "type": "boolean", "default": false }
            }
        }),
    ));
    registry.add(tool(
        "watcher_list",
        "Lista os watchers de sistema e o estado de cada um.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "watcher_delete",
        "Remove um watcher de sistema pelo nome.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
    registry.add(tool(
        "watcher_check",
        "Força uma verificação imediata de um watcher.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
    registry.add(tool(
        "watcher_pause",
        "Liga ou pausa um watcher sem apagar a configuração.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name", "enabled"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "enabled": { "type": "boolean" }
            }
        }),
    ));
}
