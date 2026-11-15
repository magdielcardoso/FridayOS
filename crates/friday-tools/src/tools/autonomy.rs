//! Self-configuration of how much Friday is allowed to do without asking.

use serde_json::{json, Value};

use crate::policy::{self, Autonomy};
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

struct AutonomyTool {
    spec: ToolSpec,
}

impl Tool for AutonomyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "autonomy_level" => {
                let level = policy::current_level();
                Ok(json!({
                    "ok": true,
                    "level": level.label(),
                    "levels": {
                        "locked": "toda ação que muda estado pede confirmação",
                        "standard": "sensíveis e críticas pedem confirmação (padrão)",
                        "full": "só ações destrutivas pedem confirmação"
                    },
                    "destructive_tools": [
                        "fs_trash", "fs_move", "fs_write", "process_kill", "power_sleep",
                        "keychain_set", "shell_run", "applescript_run", "jxa_run",
                        "app_quit", "wifi_join", "codex_login", "codex_account_switch",
                        "autonomy_set"
                    ]
                }))
            }
            "autonomy_set" => {
                let raw = args
                    .get("level")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::invalid("informe `level` (locked, standard ou full)"))?;
                let level = Autonomy::parse(raw)
                    .ok_or_else(|| ToolError::invalid("nível inválido: use locked, standard ou full"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_set": level.label() }));
                }
                policy::set_level(level)
                    .map_err(|error| ToolError::failed(format!("não consegui salvar: {error}")))?;
                Ok(json!({
                    "ok": true,
                    "level": level.label(),
                    "message": match level {
                        Autonomy::Locked => "autonomia travada: tudo que muda estado pede confirmação",
                        Autonomy::Standard => "autonomia padrão: sensíveis e críticas pedem confirmação",
                        Autonomy::Full => "acesso total: só ações destrutivas pedem confirmação",
                    }
                }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> AutonomyTool {
    AutonomyTool {
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
        "autonomy_level",
        "Diz o nível de autonomia atual da Friday e quais ações ainda pedem confirmação.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "autonomy_set",
        "Muda o nível de autonomia (locked, standard, full). Em full, só ações destrutivas pedem confirmação.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["level"],
            "additionalProperties": false,
            "properties": {
                "level": { "type": "string", "enum": ["locked", "standard", "full"] }
            }
        }),
    ));
}
