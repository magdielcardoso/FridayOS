//! Audio output routing: which device Friday's voice comes out of.

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};
use crate::swift;

struct AudioTool {
    spec: ToolSpec,
}

impl Tool for AudioTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "audio_outputs" => {
                let raw = swift::run("audio", &["list"])?;
                let list: Value = serde_json::from_str(raw.trim())
                    .map_err(|e| ToolError::failed(format!("resposta inválida do helper: {e}")))?;
                Ok(json!({ "ok": true, "outputs": list }))
            }
            "audio_output_current" => {
                let raw = swift::run("audio", &["current"])?;
                let value: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
                Ok(json!({ "ok": true, "current": value }))
            }
            "audio_output_set" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `name` com o nome do dispositivo"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_set_output": name }));
                }
                let raw = swift::run("audio", &["set", name])?;
                let value: Value = serde_json::from_str(raw.trim())
                    .map_err(|e| ToolError::failed(format!("resposta inválida do helper: {e}")))?;
                Ok(value)
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> AudioTool {
    AudioTool {
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
        "audio_outputs",
        "Lista os dispositivos de saída de áudio e qual é o padrão.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "audio_output_current",
        "Diz qual dispositivo de saída de áudio está ativo agora.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "audio_output_set",
        "Troca a saída de áudio do sistema para um dispositivo (ex.: alto-falantes internos).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Nome (ou parte do nome) do dispositivo de saída." }
            }
        }),
    ));
}
