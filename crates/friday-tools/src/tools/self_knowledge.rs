//! Metacognition: who Friday is and who the user is.
//!
//! Two small, local, persistent models — `self.json` and `user.json` under the
//! FridayOS data dir. They are injected into the system prompt so she starts
//! every conversation already knowing her own identity, limits and the person
//! she serves. No network, no clipboard, no telemetry, nothing leaves the Mac.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn self_path() -> PathBuf {
    data_dir().join("self.json")
}

fn user_path() -> PathBuf {
    data_dir().join("user.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fold(ch: char) -> char {
    match ch {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        other => other,
    }
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(fold)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn default_self() -> Value {
    json!({
        "name": "Friday",
        "role": "assistante privada desta máquina macOS, construída em FridayOS",
        "facts": [
            { "text": "Eu sou a Friday: composta, precisa, um pouco seca, nunca teatral.", "ts": 0, "source": "system" },
            { "text": "Eu falo português brasileiro por padrão e trato o usuário por 'senhor' quando cabe.", "ts": 0, "source": "system" },
            { "text": "Eu sou o próprio sistema: vejo a tela, controlo apps, arquivos, shell, rede, calendário e voz.", "ts": 0, "source": "system" },
            { "text": "Minha voz é Piper Dii, neural e local; a transcrição é on-device; o raciocínio vem da assinatura Codex.", "ts": 0, "source": "system" }
        ],
        "updated_at": 0
    })
}

fn default_user() -> Value {
    json!({
        "name": "senhor",
        "role": "",
        "facts": [],
        "updated_at": 0
    })
}

fn load(path: &Path, fallback: fn() -> Value) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return fallback();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return fallback();
    };
    if !value.is_object() {
        return fallback();
    }
    value
}

fn write(path: &Path, value: &Value) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| ToolError::failed(error.to_string()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
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
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    drop(file);
    std::fs::rename(&tmp, path).map_err(|error| ToolError::failed(error.to_string()))
}

fn upsert_fact(model: &mut Value, text: &str, source: &str) -> bool {
    let key = normalize(text);
    if key.is_empty() {
        return false;
    }
    let facts = model
        .get_mut("facts")
        .and_then(Value::as_array_mut)
        .map(|facts| facts);
    let Some(facts) = facts else {
        return false;
    };
    if let Some(existing) = facts.iter_mut().find(|fact| {
        fact.get("text")
            .and_then(Value::as_str)
            .map(normalize)
            .map(|old| old == key)
            .unwrap_or(false)
    }) {
        existing["ts"] = json!(now());
        existing["source"] = json!(source);
        return false;
    }
    facts.push(json!({
        "text": text,
        "ts": now(),
        "source": source,
    }));
    const MAX: usize = 100;
    if facts.len() > MAX {
        let extra = facts.len() - MAX;
        facts.drain(0..extra);
    }
    true
}

fn summary(model: &Value) -> String {
    let name = model.get("name").and_then(Value::as_str).unwrap_or("");
    let role = model.get("role").and_then(Value::as_str).unwrap_or("");
    let mut out = String::new();
    if !name.is_empty() {
        out.push_str(&format!("Nome: {name}. "));
    }
    if !role.is_empty() {
        out.push_str(&format!("Papel: {role}. "));
    }
    if let Some(facts) = model.get("facts").and_then(Value::as_array) {
        for fact in facts.iter().rev().take(20) {
            if let Some(text) = fact.get("text").and_then(Value::as_str) {
                out.push_str(&format!("- {text}\n"));
            }
        }
    }
    out
}

struct ToolSpecified {
    spec: ToolSpec,
}

impl Tool for ToolSpecified {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "self_describe" => Ok(json!({
                "ok": true,
                "self": load(&self_path(), default_self),
                "summary": summary(&load(&self_path(), default_self)),
            })),
            "user_profile" => Ok(json!({
                "ok": true,
                "user": load(&user_path(), default_user),
                "summary": summary(&load(&user_path(), default_user)),
            })),
            "metacognition" => {
                let self_model = load(&self_path(), default_self);
                let user_model = load(&user_path(), default_user);
                let mode = std::fs::read_to_string(data_dir().join("mode.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| value.get("mode").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "normal".into());
                let missions = std::fs::read_dir(data_dir().join("missions"))
                    .map(|entries| entries.filter_map(Result::ok).count())
                    .unwrap_or(0);
                let autonomy = std::fs::read_to_string(data_dir().join("autonomy.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| value.get("level").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "standard".into());
                Ok(json!({
                    "ok": true,
                    "self": summary(&self_model),
                    "user": summary(&user_model),
                    "context": {
                        "mode": mode,
                        "autonomy": autonomy,
                        "missions": missions,
                        "voice": "Piper Dii (local)",
                        "reasoning": "Codex (gpt-6-luna)",
                    },
                }))
            }
            "self_update" | "user_update" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `text`"))?;
                let is_self = self.spec.name == "self_update";
                let path = if is_self { self_path() } else { user_path() };
                let mut model = load(&path, if is_self { default_self } else { default_user });
                if let Some(name) = args.get("name").and_then(Value::as_str) {
                    let clean = name.trim();
                    if (1..=48).contains(&clean.chars().count()) {
                        model["name"] = json!(clean);
                    }
                }
                if let Some(role) = args.get("role").and_then(Value::as_str) {
                    let clean = role.trim();
                    if (1..=160).contains(&clean.chars().count()) {
                        model["role"] = json!(clean);
                    }
                }
                let source = args
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let added = upsert_fact(&mut model, text, source);
                model["updated_at"] = json!(now());
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_update": text, "added": added }));
                }
                write(&path, &model)?;
                Ok(json!({ "ok": true, "added": added, "model": model }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn register_tool(
    registry: &mut Registry,
    name: &'static str,
    summary: &'static str,
    risk: crate::policy::Risk,
) {
    registry.add(ToolSpecified {
        spec: ToolSpec {
            name,
            summary,
            risk,
            args: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "name": { "type": "string" },
                    "role": { "type": "string" },
                    "source": { "type": "string" }
                },
            }),
        },
    });
}

pub fn register(registry: &mut Registry) {
    register_tool(
        registry,
        "self_describe",
        "Descreve quem a Friday é: identidade, papéis, limites e fatos estáveis.",
        crate::policy::Risk::ReadOnly,
    );
    register_tool(
        registry,
        "self_update",
        "Atualiza a identidade da Friday quando o usuário corrige ou define quem ela é.",
        crate::policy::Risk::Reversible,
    );
    register_tool(
        registry,
        "user_profile",
        "Descreve quem é o usuário: nome, papel, preferências e fatos duráveis.",
        crate::policy::Risk::ReadOnly,
    );
    register_tool(
        registry,
        "user_update",
        "Registra quem o usuário é ou uma preferência durável que ele declarou.",
        crate::policy::Risk::Reversible,
    );
    register_tool(
        registry,
        "metacognition",
        "Visão condensada de quem a Friday é, quem é o usuário e do estado atual do sistema.",
        crate::policy::Risk::ReadOnly,
    );
}
