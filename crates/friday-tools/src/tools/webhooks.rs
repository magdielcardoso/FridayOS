//! Local webhooks and scheduled routines that wake Friday on their own.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn hooks_path() -> PathBuf {
    data_dir().join("webhooks.json")
}

fn routines_path() -> PathBuf {
    data_dir().join("routines.json")
}

fn port() -> u16 {
    std::env::var("FRIDAY_HOOK_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8787)
}

fn read_list(path: PathBuf) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn write_list(path: PathBuf, items: &[Value]) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::failed(e.to_string()))?;
    }
    let text = serde_json::to_vec_pretty(&Value::Array(items.to_vec()))
        .map_err(|e| ToolError::failed(e.to_string()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| ToolError::failed(e.to_string()))?;
    std::fs::rename(tmp, path).map_err(|e| ToolError::failed(e.to_string()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|value| value.as_str()).map(str::trim).filter(|v| !v.is_empty())
}

fn parse_at_epoch(value: &str) -> Option<u64> {
    if let Ok(epoch) = value.parse::<u64>() {
        return Some(epoch);
    }
    let output = std::process::Command::new("/bin/date")
        .args(["-j", "-f", "%Y-%m-%dT%H:%M", value, "+%s"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

struct AutomationTool {
    spec: ToolSpec,
}

impl Tool for AutomationTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "webhook_create" => {
                let name = arg_str(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let prompt = arg_str(args, "prompt").ok_or_else(|| ToolError::invalid("informe `prompt`"))?;
                let secret = arg_str(args, "secret").unwrap_or("");
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_create": name }));
                }
                let mut hooks = read_list(hooks_path());
                hooks.retain(|hook| hook.get("name").and_then(|v| v.as_str()) != Some(name));
                hooks.push(json!({
                    "name": name,
                    "prompt": prompt,
                    "secret": secret,
                    "created": now(),
                }));
                write_list(hooks_path(), &hooks)?;
                Ok(json!({
                    "ok": true,
                    "name": name,
                    "url": format!("http://127.0.0.1:{}/hook/{}", port(), name),
                    "method": "POST",
                    "note": "a Friday precisa estar aberta para receber o evento"
                }))
            }
            "webhook_list" => Ok(json!({
                "ok": true,
                "port": port(),
                "webhooks": read_list(hooks_path())
            })),
            "webhook_delete" => {
                let name = arg_str(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_delete": name }));
                }
                let mut hooks = read_list(hooks_path());
                let before = hooks.len();
                hooks.retain(|hook| hook.get("name").and_then(|v| v.as_str()) != Some(name));
                write_list(hooks_path(), &hooks)?;
                Ok(json!({ "ok": true, "removed": before - hooks.len() }))
            }
            "webhook_test" => {
                let name = arg_str(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let payload = arg_str(args, "payload").unwrap_or("{\"teste\":true}");
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_post": name }));
                }
                let message = format!(
                    "POST /hook/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    name,
                    payload.len(),
                    payload
                );
                let mut stream = TcpStream::connect(("127.0.0.1", port()))
                    .map_err(|error| ToolError::failed(format!("conectar no webhook: {error}")))?;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                stream.write_all(message.as_bytes()).map_err(|e| ToolError::failed(e.to_string()))?;
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                Ok(json!({ "ok": response.contains("200 OK"), "response": response.lines().next().unwrap_or("") }))
            }
            "routine_create" => {
                let name = arg_str(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let prompt = arg_str(args, "prompt").ok_or_else(|| ToolError::invalid("informe `prompt`"))?;
                let schedule = if let Some(minutes) = args.get("interval_minutes").and_then(|v| v.as_u64()) {
                    json!({ "type": "interval", "minutes": minutes.max(1) })
                } else if let Some(time) = arg_str(args, "daily_at") {
                    json!({ "type": "daily", "time": time })
                } else if let Some(time) = arg_str(args, "weekly_at") {
                    let weekday = arg_str(args, "weekday").ok_or_else(|| ToolError::invalid("informe `weekday`"))?;
                    json!({ "type": "weekly", "time": time, "weekday": weekday })
                } else if let Some(at) = arg_str(args, "at") {
                    let epoch = parse_at_epoch(at).ok_or_else(|| {
                        ToolError::invalid("use `at` no formato YYYY-MM-DDTHH:MM (hora local) ou epoch")
                    })?;
                    json!({ "type": "at", "epoch": epoch })
                } else {
                    return Err(ToolError::invalid(
                        "informe `interval_minutes`, `daily_at` (HH:MM), `weekly_at`+`weekday` ou `at`",
                    ));
                };
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_create": name, "schedule": schedule }));
                }
                let mut routines = read_list(routines_path());
                routines.retain(|routine| routine.get("name").and_then(|v| v.as_str()) != Some(name));
                routines.push(json!({
                    "name": name,
                    "prompt": prompt,
                    "schedule": schedule,
                    "enabled": true,
                    "last_run": now(),
                    "last_day": "",
                    "fired": false,
                    "created": now(),
                }));
                write_list(routines_path(), &routines)?;
                Ok(json!({ "ok": true, "name": name, "schedule": schedule }))
            }
            "routine_list" => Ok(json!({ "ok": true, "routines": read_list(routines_path()) })),
            "routine_delete" => {
                let name = arg_str(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_delete": name }));
                }
                let mut routines = read_list(routines_path());
                let before = routines.len();
                routines.retain(|routine| routine.get("name").and_then(|v| v.as_str()) != Some(name));
                write_list(routines_path(), &routines)?;
                Ok(json!({ "ok": true, "removed": before - routines.len() }))
            }
            "routine_run_now" => {
                let name = arg_str(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let mut routines = read_list(routines_path());
                if let Some(routine) = routines
                    .iter_mut()
                    .find(|routine| routine.get("name").and_then(|v| v.as_str()) == Some(name))
                {
                    if ctx.dry_run {
                        return Ok(json!({ "dry_run": true, "would_run": name }));
                    }
                    routine["last_run"] = json!(0);
                    routine["last_day"] = json!("");
                    routine["fired"] = json!(false);
                    routine["enabled"] = json!(true);
                    write_list(routines_path(), &routines)?;
                    Ok(json!({ "ok": true, "queued": name, "note": "o agendador roda em até 10 s" }))
                } else {
                    Err(ToolError::failed(format!("rotina `{name}` não existe")))
                }
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> AutomationTool {
    AutomationTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "webhook_create",
        "Cria um webhook local (POST http://127.0.0.1:8787/hook/nome) que acorda a Friday com um prompt.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name", "prompt"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "prompt": { "type": "string", "description": "O que a Friday deve fazer ao receber o evento." },
                "secret": { "type": "string", "description": "Opcional: exige header X-Friday-Secret." }
            }
        }),
    ));
    registry.add(tool(
        "webhook_list",
        "Lista webhooks locais configurados.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "webhook_delete",
        "Remove um webhook local pelo nome.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
    registry.add(tool(
        "webhook_test",
        "Dispara um webhook local para testar.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "payload": { "type": "string" }
            }
        }),
    ));
    registry.add(tool(
        "routine_create",
        "Cria uma rotina agendada que acorda a Friday: intervalo, diária, semanal ou data específica.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name", "prompt"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "prompt": { "type": "string" },
                "interval_minutes": { "type": "integer", "minimum": 1 },
                "daily_at": { "type": "string", "description": "HH:MM no horário local" },
                "weekly_at": { "type": "string", "description": "HH:MM" },
                "weekday": { "type": "string", "description": "1-7 (segunda=1) ou mon..sun" },
                "at": { "type": "string", "description": "YYYY-MM-DDTHH:MM ou epoch" }
            }
        }),
    ));
    registry.add(tool(
        "routine_list",
        "Lista as rotinas agendadas e quando rodaram pela última vez.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "routine_delete",
        "Remove uma rotina agendada pelo nome.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
    registry.add(tool(
        "routine_run_now",
        "Marca uma rotina para rodar no próximo ciclo do agendador (até 10 s).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
}
