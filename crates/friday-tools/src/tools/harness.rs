//! Harness control surface: status, self-configuration and verification reads.
//!
//! These tools expose the local verification/governance layer that wraps every
//! mutating call (see [`crate::registry::recent_verifications`]). Everything is
//! file-backed under the FridayOS data dir: no network, no telemetry, no
//! secrets. `harness.json` is written atomically and unknown fields are kept.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::registry::{
    recent_verifications, Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec,
};

const VERSION: &str = "1.0.0";
const DEFAULT_CONTEXT_BUDGET: u64 = 8_000;
const MAX_SKILLS: usize = 64;
const MAX_STATUS_CHECKS: usize = 5;

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn config_path() -> PathBuf {
    data_dir().join("harness.json")
}

fn load_object(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({}))
}

fn load_str(path: &Path, key: &str, fallback: &str) -> String {
    load_object(path)
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn default_config() -> Value {
    json!({
        "version": VERSION,
        "proactivity": 1,
        "verification": "light",
        "context_budget": DEFAULT_CONTEXT_BUDGET,
        "subagent_depth": 1,
        "skills": [],
        "updated_at": 0,
    })
}

/// Loads the persisted config on top of defaults so unknown fields survive.
fn load_config() -> Value {
    let mut config = default_config();
    let stored = load_object(&config_path());
    if let Value::Object(map) = stored {
        for (key, value) in map {
            config[key.as_str()] = value;
        }
    }
    config
}

fn write_atomic(path: &Path, value: &Value) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ToolError::failed(error.to_string()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|error| ToolError::failed(error.to_string()))?;
    file.write_all(value.to_string().as_bytes())
        .map_err(|error| ToolError::failed(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    drop(file);
    fs::rename(&tmp, path).map_err(|error| ToolError::failed(error.to_string()))
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

fn bounded_int(args: &Value, key: &str, min: i64, max: i64) -> Result<Option<i64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number
                .as_i64()
                .ok_or_else(|| ToolError::invalid(format!("`{key}` deve ser um inteiro")))?;
            if value < min || value > max {
                return Err(ToolError::invalid(format!(
                    "`{key}` deve estar entre {min} e {max}"
                )));
            }
            Ok(Some(value))
        }
        Some(_) => Err(ToolError::invalid(format!("`{key}` deve ser um inteiro"))),
    }
}

fn parse_skills(args: &Value) -> Result<Option<Vec<String>>, ToolError> {
    let Some(value) = args.get("skills") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(items) = value.as_array() else {
        return Err(ToolError::invalid("`skills` deve ser uma lista de nomes"));
    };
    let mut skills: Vec<String> = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            return Err(ToolError::invalid("`skills` deve conter apenas strings"));
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if name.chars().count() > 64 {
            return Err(ToolError::invalid("nome de skill muito longo"));
        }
        if !skills.iter().any(|existing| existing == name) {
            skills.push(name.to_string());
        }
        if skills.len() >= MAX_SKILLS {
            break;
        }
    }
    Ok(Some(skills))
}

// ---------------------------------------------------------------------------
// Tool plumbing
// ---------------------------------------------------------------------------

struct HarnessTool {
    spec: ToolSpec,
}

impl Tool for HarnessTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "harness_status" => harness_status(),
            "harness_update" => harness_update(args, ctx),
            "run_verification" => run_verification(args),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> HarnessTool {
    HarnessTool {
        spec: ToolSpec {
            name,
            summary,
            risk,
            args,
        },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "harness_status",
        "Relata o estado do harness FridayOS: versão, autonomia, modo, orçamento de contexto e verificações recentes.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "harness_update",
        "Ajusta o harness local (proatividade, verificação, orçamento de contexto, profundidade de subagentes e skills). Reversível; grava harness.json de forma atômica.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "proactivity": { "type": "integer", "minimum": 0, "maximum": 3 },
                "verification": { "type": "string", "enum": ["off", "light", "strict"] },
                "context_budget": { "type": "integer", "minimum": 2000, "maximum": 20000 },
                "subagent_depth": { "type": "integer", "minimum": 0, "maximum": 3 },
                "skills": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            }
        }),
    ));
    registry.add(tool(
        "run_verification",
        "Lê os registros de verificação mais recentes do harness (padrão: 20, máximo: 500).",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500,
                    "default": 20
                }
            }
        }),
    ));
}

// ---------------------------------------------------------------------------
// harness_status
// ---------------------------------------------------------------------------

fn harness_status() -> ToolResult {
    let config = load_config();
    let context_budget = config
        .get("context_budget")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_CONTEXT_BUDGET);
    let autonomy = load_str(&data_dir().join("autonomy.json"), "level", "standard");
    let mode = load_str(&data_dir().join("mode.json"), "mode", "normal");

    let verification_path = data_dir().join("verification.jsonl");
    let verification_count = fs::read_to_string(&verification_path)
        .map(|text| {
            text.lines()
                .filter(|line| serde_json::from_str::<Value>(line).is_ok())
                .count()
        })
        .unwrap_or(0);
    let last_checks: Vec<Value> = recent_verifications(MAX_STATUS_CHECKS)
        .iter()
        .map(|record| {
            json!({
                "tool": record.get("tool").cloned().unwrap_or(Value::Null),
                "status": record.get("status").cloned().unwrap_or(Value::Null),
                "checks": record.get("checks").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    Ok(json!({
        "ok": true,
        "version": VERSION,
        "autonomy": autonomy,
        "mode": mode,
        "context_budget": context_budget,
        "verification_count": verification_count,
        "last_checks": last_checks,
    }))
}

// ---------------------------------------------------------------------------
// harness_update
// ---------------------------------------------------------------------------

fn harness_update(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let proactivity = bounded_int(args, "proactivity", 0, 3)?;
    let subagent_depth = bounded_int(args, "subagent_depth", 0, 3)?;
    let context_budget = bounded_int(args, "context_budget", 2_000, 20_000)?;
    let skills = parse_skills(args)?;

    let verification = match args.get("verification") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => {
            let value = value.trim().to_lowercase();
            if !matches!(value.as_str(), "off" | "light" | "strict") {
                return Err(ToolError::invalid(
                    "`verification` deve ser off, light ou strict",
                ));
            }
            Some(value)
        }
        Some(_) => return Err(ToolError::invalid("`verification` deve ser uma string")),
    };

    if proactivity.is_none()
        && subagent_depth.is_none()
        && context_budget.is_none()
        && verification.is_none()
        && skills.is_none()
    {
        return Err(ToolError::invalid("informe ao menos um campo para atualizar"));
    }

    let mut config = load_config();
    if let Some(value) = proactivity {
        config["proactivity"] = json!(value);
    }
    if let Some(value) = subagent_depth {
        config["subagent_depth"] = json!(value);
    }
    if let Some(value) = context_budget {
        config["context_budget"] = json!(value);
    }
    if let Some(value) = verification {
        config["verification"] = json!(value);
    }
    if let Some(value) = skills {
        config["skills"] = json!(value);
    }
    config["updated_at"] = json!(now_secs());

    if ctx.dry_run {
        return Ok(json!({ "dry_run": true, "would_update": config }));
    }

    write_atomic(&config_path(), &config)?;
    Ok(json!({ "ok": true, "config": config }))
}

// ---------------------------------------------------------------------------
// run_verification
// ---------------------------------------------------------------------------

fn run_verification(args: &Value) -> ToolResult {
    let limit = match args.get("limit") {
        None | Some(Value::Null) => 20,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| ToolError::invalid("`limit` deve ser um inteiro positivo"))?
            .clamp(1, 500) as usize,
        Some(_) => return Err(ToolError::invalid("`limit` deve ser um inteiro positivo")),
    };
    let entries = recent_verifications(limit);
    Ok(json!({
        "ok": true,
        "count": entries.len(),
        "entries": entries,
    }))
}
