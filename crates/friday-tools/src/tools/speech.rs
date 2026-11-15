//! Terminal speech: `/usr/bin/say`, as an explicit fallback the agent can call.

use std::process::Command;

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

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> SayTool {
    SayTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "say",
        "Fala um texto pelo comando `say` do macOS (voz local, ex.: Luciana em pt-BR).",
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
