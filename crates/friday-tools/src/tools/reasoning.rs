//! Persistent Codex reasoning effort selected through Friday's MCP tools.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/reasoning.json")
}

fn configured_effort() -> String {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("effort").and_then(Value::as_str).map(str::to_owned))
        .filter(|effort| EFFORTS.contains(&effort.as_str()))
        .unwrap_or_else(|| "low".to_string())
}

struct ReasoningTool {
    spec: ToolSpec,
}

impl Tool for ReasoningTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "reasoning_get" => Ok(json!({
                "ok": true,
                "effort": configured_effort(),
                "applies": "next user turn",
                "config_path": config_path().display().to_string()
            })),
            "reasoning_set" => {
                let effort = args
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .ok_or_else(|| ToolError::invalid("informe effort"))?;
                if !EFFORTS.contains(&effort) {
                    return Err(ToolError::invalid(format!(
                        "esforço inválido; use um destes valores: {}",
                        EFFORTS.join(", ")
                    )));
                }
                if ctx.dry_run {
                    return Ok(json!({
                        "dry_run": true,
                        "would_set": effort,
                        "applies": "next user turn"
                    }));
                }

                let path = config_path();
                let parent = path
                    .parent()
                    .ok_or_else(|| ToolError::failed("caminho de configuração inválido"))?;
                std::fs::create_dir_all(parent).map_err(|error| ToolError::failed(error.to_string()))?;
                let temporary = path.with_extension("json.tmp");
                let payload = serde_json::to_vec_pretty(&json!({ "effort": effort }))
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                std::fs::write(&temporary, payload)
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                std::fs::rename(&temporary, &path)
                    .map_err(|error| ToolError::failed(error.to_string()))?;

                Ok(json!({
                    "ok": true,
                    "effort": effort,
                    "applies": "next user turn",
                    "config_path": path.display().to_string()
                }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn make_tool(name: &'static str, summary: &'static str, risk: Risk, args: Value) -> ReasoningTool {
    ReasoningTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(make_tool(
        "reasoning_get",
        "Diz o nível de raciocínio configurado para os próximos turnos do Codex.",
        Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(make_tool(
        "reasoning_set",
        "Configura o esforço de raciocínio do Codex para a próxima solicitação do usuário.",
        Risk::Reversible,
        json!({
            "type": "object",
            "required": ["effort"],
            "additionalProperties": false,
            "properties": {
                "effort": {
                    "type": "string",
                    "enum": EFFORTS,
                    "description": "low, medium, high, xhigh ou max"
                }
            }
        }),
    ));
}
