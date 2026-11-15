//! Voice preferences: which ChatGPT voice Friday speaks with.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/voice.json")
}

fn read_config() -> Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}))
}

/// The app binary that can audition a voice; the MCP server lives in the same
/// bundle, so look next to ourselves first.
fn app_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("FridayOS"));
            candidates.push(dir.join("friday-app"));
        }
    }
    candidates.push(PathBuf::from("/Applications/FridayOS.app/Contents/MacOS/FridayOS"));
    candidates.into_iter().find(|path| path.is_file())
}

struct VoiceTool {
    spec: ToolSpec,
}

impl Tool for VoiceTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "voice_config" => {
                let config = read_config();
                let voice = std::env::var("FRIDAY_VOICE")
                    .ok()
                    .or_else(|| config.get("voice").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "cove".into());
                Ok(json!({
                    "ok": true,
                    "voice": voice,
                    "language": config
                        .get("language")
                        .and_then(Value::as_str)
                        .unwrap_or("pt-BR"),
                    "config_path": config_path().display().to_string(),
                }))
            }
            "voice_list" => Ok(json!({
                "ok": true,
                "voices": [
                    "viola", "rio", "marin", "cedar", "cove", "juniper", "breeze",
                    "ember", "maple", "sol", "spruce", "vale", "arbor", "birch"
                ],
                "note": "nomes desconhecidos caem na voz padrão da conta"
            })),
            "voice_set" => {
                let voice = args
                    .get("voice")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `voice`"))?;
                let language = args.get("language").and_then(Value::as_str);
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_set": voice }));
                }
                let dir = config_path().parent().map(PathBuf::from).unwrap_or_default();
                std::fs::create_dir_all(&dir).map_err(|e| ToolError::failed(e.to_string()))?;
                let mut value = read_config();
                value["voice"] = json!(voice);
                if let Some(language) = language {
                    value["language"] = json!(language);
                }
                std::fs::write(
                    config_path(),
                    serde_json::to_vec_pretty(&value).map_err(|e| ToolError::failed(e.to_string()))?,
                )
                .map_err(|e| ToolError::failed(e.to_string()))?;
                Ok(json!({
                    "ok": true,
                    "voice": voice,
                    "language": language.unwrap_or("pt-BR"),
                    "message": "voz salva; vale para as próximas respostas"
                }))
            }
            "voice_audition" => {
                let voice = args
                    .get("voice")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `voice`"))?;
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("Bom dia, senhor. Esta é a minha voz.");
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_speak": text, "voice": voice }));
                }
                let binary = app_binary().ok_or_else(|| {
                    ToolError::failed("não encontrei o binário da Friday para tocar a amostra")
                })?;
                let mut child = Command::new(binary)
                    .arg("--say-voice")
                    .arg(voice)
                    .arg(text)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| ToolError::failed(format!("não consegui falar: {e}")))?;
                let deadline = Instant::now() + Duration::from_secs(90);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            return Ok(json!({
                                "ok": status.success(),
                                "voice": voice,
                                "status": format!("{status}")
                            }))
                        }
                        Ok(None) => {}
                        Err(error) => return Err(ToolError::failed(error.to_string())),
                    }
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        return Err(ToolError::failed("a amostra de voz demorou demais"));
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> VoiceTool {
    VoiceTool {
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
        "voice_config",
        "Diz qual voz do ChatGPT a Friday está usando (padrão: cove) e em que idioma.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "voice_list",
        "Lista os nomes de voz do ChatGPT que a Friday conhece.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "voice_set",
        "Define a voz do ChatGPT usada pela Friday (orbit, ember, maple, juniper, breeze, cove, glimmer, vale, fathom).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["voice"],
            "additionalProperties": false,
            "properties": {
                "voice": { "type": "string" },
                "language": { "type": "string", "default": "pt-BR" }
            }
        }),
    ));
    registry.add(tool(
        "voice_audition",
        "Fala uma amostra com uma voz específica para o usuário escolher.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["voice"],
            "additionalProperties": false,
            "properties": {
                "voice": { "type": "string" },
                "text": { "type": "string" }
            }
        }),
    ));
}
