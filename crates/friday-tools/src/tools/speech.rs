//! Terminal speech: `/usr/bin/say`, as an explicit fallback the agent can call.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

struct SayTool {
    spec: ToolSpec,
}

impl Tool for SayTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "say" => {
                let text = args
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `text`"))?;
                let voice = args
                    .get("voice")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Luciana");
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_say": text, "voice": voice }));
                }
                // Local voice is a last resort: only when the ChatGPT
                // read-aloud quota is exhausted.
                if !chatgpt_voice_blocked() {
                    return Err(ToolError::Denied(
                        "a voz do ChatGPT está disponível; não use `say` (o app fala sozinho)"
                            .to_string(),
                    ));
                }
                let status = Command::new("/usr/bin/say")
                    .arg("-v")
                    .arg(voice)
                    .arg(text)
                    .status()
                    .map_err(|error| ToolError::failed(format!("say: {error}")))?;
                Ok(json!({ "ok": status.success(), "voice": voice, "status": status.to_string() }))
            }
            "say_voices" => {
                let output = Command::new("/usr/bin/say")
                    .arg("-v")
                    .arg("?")
                    .output()
                    .map_err(|error| ToolError::failed(format!("say: {error}")))?;
                let voices: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| line.contains("pt_BR") || line.contains("pt-BR"))
                    .map(|line| line.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
                    .collect();
                Ok(json!({ "ok": true, "voices": voices }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn chatgpt_voice_blocked() -> bool {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = PathBuf::from(home)
        .join("Library/Application Support/FridayOS/tts-usage.json");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let blocked_until = value.get("blocked_until").and_then(|v| v.as_u64()).unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    blocked_until > now
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> SayTool {
    SayTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "say",
        "Fala pelo `say` do macOS APENAS quando a voz do ChatGPT estiver bloqueada por limite de requisições.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["text"],
            "additionalProperties": false,
            "properties": {
                "text": { "type": "string" },
                "voice": { "type": "string", "default": "Luciana" }
            }
        }),
    ));
    registry.add(tool(
        "say_voices",
        "Lista as vozes pt-BR instaladas no macOS para o comando say.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
}
