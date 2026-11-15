//! Query and inspect the tool audit trail.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

struct AuditTool {
    spec: ToolSpec,
}

impl Tool for AuditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolResult {
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let wanted_tool = args
            .get("tool")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let since = args.get("since_secs").and_then(|value| value.as_u64());
        let now = now_secs();

        let mut entries: Vec<Value> = Vec::new();
        for name in ["audit.jsonl", "audit.1.jsonl"] {
            let Ok(text) = std::fs::read_to_string(data_dir().join(name)) else {
                continue;
            };
            for line in text.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let Some(tool) = &wanted_tool {
                    if value.get("tool").and_then(Value::as_str) != Some(tool.as_str()) {
                        continue;
                    }
                }
                if let Some(since) = since {
                    let ts = value.get("ts").and_then(Value::as_u64).unwrap_or(0);
                    if ts < now.saturating_sub(since) {
                        continue;
                    }
                }
                entries.push(value);
            }
        }
        entries.sort_by_key(|entry| entry.get("ts").and_then(Value::as_u64).unwrap_or(0));
        let total = entries.len();
        let tail: Vec<Value> = entries.into_iter().rev().take(limit).collect();
        Ok(json!({
            "ok": true,
            "total": total,
            "returned": tail.len(),
            "entries": tail,
        }))
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(AuditTool {
        spec: ToolSpec {
            name: "audit_query",
            summary: "Consulta o histórico de ferramentas executadas (quem chamou o quê, quando e com quais argumentos), do mais recente para o mais antigo.",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 },
                    "tool": { "type": "string", "description": "filtrar por nome da ferramenta" },
                    "since_secs": { "type": "integer", "minimum": 1, "description": "somente entradas dos últimos N segundos" }
                }
            }),
        },
    });
}
