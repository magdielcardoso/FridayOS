//! Self-diagnosis: what broke, when, and what the environment looks like now.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn read_tail(path: &PathBuf, lines: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|line| line.to_string()).collect()
}

fn is_error(line: &str) -> bool {
    let lower = line.to_lowercase();
    [
        "erro",
        "falhou",
        "falha",
        "indisponível",
        "429",
        "denied",
        "timeout",
        "panic",
        "não consegui",
        "não entendi",
        "watchdog",
        "desconect",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

struct DiagnoseTool {
    spec: ToolSpec,
}

impl Tool for DiagnoseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolResult {
        let lines = args
            .get("lines")
            .and_then(|value| value.as_u64())
            .unwrap_or(300)
            .clamp(20, 2000) as usize;

        let log = home().join("Library/Logs/FridayOS.log");
        let tail = read_tail(&log, lines);
        let errors: Vec<&String> = tail.iter().filter(|line| is_error(line)).collect();

        let mut issues: Vec<String> = Vec::new();
        if !log.is_file() {
            issues.push("log do app não existe".into());
        }
        if std::process::Command::new("/bin/zsh")
            .args(["-lc", "command -v codex"])
            .output()
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            issues.push("CLI do Codex não encontrado no PATH de login".into());
        }
        if !home().join(".codex/auth.json").is_file() {
            issues.push("auth padrão do Codex ausente".into());
        }
        for socket in ["screen.sock", "overlay.sock"] {
            if !data_dir().join(socket).exists() {
                issues.push(format!("socket {socket} ausente: a janela da Friday está aberta?"));
            }
        }
        let usage = std::fs::read_to_string(data_dir().join("tts-usage.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let blocked_until = usage
            .get("blocked_until")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if blocked_until > now {
            issues.push(format!(
                "voz bloqueada por mais {} min (limite do read-aloud)",
                (blocked_until - now) / 60
            ));
        }
        let autonomy = std::fs::read_to_string(data_dir().join("autonomy.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("level").and_then(|l| l.as_str()).map(str::to_string))
            .unwrap_or_else(|| "standard".into());
        let account = std::fs::read_to_string(data_dir().join("codex-accounts/active"))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "default".into());

        let disk = std::process::Command::new("/bin/df")
            .args(["-h", "/"])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
            .unwrap_or_default();
        let app_running = std::process::Command::new("/usr/bin/pgrep")
            .args(["-f", "FridayOS.app/Contents/MacOS/FridayOS"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);

        Ok(json!({
            "ok": true,
            "health": {
                "app_running": app_running,
                "account": account,
                "autonomy": autonomy,
                "voice_blocked_until": blocked_until,
                "voice_requests_last_hour": usage.get("requests").cloned().unwrap_or(Value::Null),
                "codex_home": std::env::var("CODEX_HOME").unwrap_or_else(|_| "~/.codex".into()),
                "disk": disk.lines().last().unwrap_or("").trim(),
            },
            "issues": issues,
            "recent_errors": errors.iter().rev().take(40).map(|line| line.to_string()).collect::<Vec<_>>(),
            "log_tail": tail.iter().rev().take(60).map(|line| line.to_string()).collect::<Vec<_>>(),
            "hint": "cruze recent_errors com log_tail; issues lista o que está faltando agora"
        }))
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(DiagnoseTool {
        spec: ToolSpec {
            name: "diagnose",
            summary: "Analisa os logs e o ambiente da Friday, lista erros recentes e o que está faltando agora.",
            risk: crate::policy::Risk::ReadOnly,
            args: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "lines": { "type": "integer", "minimum": 20, "maximum": 2000, "default": 300 }
                }
            }),
        },
    });
}

#[allow(dead_code)]
fn _unused(error: ToolError) {}
