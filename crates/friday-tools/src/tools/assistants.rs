//! Permanent assistants Friday can create by name and delegate tasks to.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/assistants")
}

fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() { "assistente".into() } else { trimmed }
}

fn path_for(name: &str) -> PathBuf {
    dir().join(format!("{}.json", slug(name)))
}

fn load(name: &str) -> Result<Value, ToolError> {
    let raw = std::fs::read_to_string(path_for(name))
        .map_err(|_| ToolError::failed(format!("assistente `{}` não existe", slug(name))))?;
    serde_json::from_str(&raw).map_err(|error| ToolError::failed(error.to_string()))
}

fn codex_binary() -> Option<PathBuf> {
    if let Ok(output) = Command::new("/bin/zsh").args(["-lc", "command -v codex"]).output() {
        if output.status.success() {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [home.join(".local/bin/codex"), PathBuf::from("/opt/homebrew/bin/codex")]
        .into_iter()
        .find(|path| path.is_file())
}

fn login_path() -> String {
    Command::new("/bin/zsh")
        .args(["-lc", "printf %s \"$PATH\""])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".into())
}

fn delegate(assistant: &Value, task: &str, timeout_secs: u64) -> Result<String, ToolError> {
    let binary = codex_binary().ok_or_else(|| ToolError::failed("CLI do Codex não encontrado"))?;
    let name = assistant.get("name").and_then(|v| v.as_str()).unwrap_or("assistente");
    let role = assistant.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let instructions = assistant.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
    let prompt = format!(
        "Você é {name}, um assistente permanente criado pela Friday.\n\
         Papel: {role}\n\
         Instruções: {instructions}\n\n\
         Tarefa delegada pela Friday: {task}\n\n\
         Responda de forma objetiva e completa, em português."
    );

    let out = std::env::temp_dir().join(format!("friday-assistant-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut command = Command::new(binary);
    command
        .args(["exec", "--skip-git-repo-check", "-C", &home, "-o"])
        .arg(&out)
        .args([
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "sandbox_mode=\"workspace-write\"",
        ])
        .arg(&prompt)
        .env("PATH", login_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        command.env("CODEX_HOME", codex_home);
    }

    let mut child = command
        .spawn()
        .map_err(|error| ToolError::failed(format!("codex exec: {error}")))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(30, 600));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let answer = std::fs::read_to_string(&out).unwrap_or_default();
                let _ = std::fs::remove_file(&out);
                if answer.trim().is_empty() {
                    return Err(ToolError::failed(format!(
                        "o assistente terminou sem resposta ({status})"
                    )));
                }
                return Ok(answer.trim().to_string());
            }
            Ok(None) => {}
            Err(error) => return Err(ToolError::failed(error.to_string())),
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::failed("o assistente demorou demais"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct AssistantTool {
    spec: ToolSpec,
}

impl Tool for AssistantTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "assistant_create" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let instructions = args.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_create": slug(name) }));
                }
                std::fs::create_dir_all(dir()).map_err(|e| ToolError::failed(e.to_string()))?;
                let record = json!({
                    "name": name,
                    "slug": slug(name),
                    "role": role,
                    "instructions": instructions,
                    "created": now_secs(),
                });
                std::fs::write(
                    path_for(name),
                    serde_json::to_vec_pretty(&record).map_err(|e| ToolError::failed(e.to_string()))?,
                )
                .map_err(|e| ToolError::failed(e.to_string()))?;
                Ok(json!({ "ok": true, "assistant": record }))
            }
            "assistant_list" => {
                let mut list = Vec::new();
                if let Ok(entries) = std::fs::read_dir(dir()) {
                    for entry in entries.filter_map(|entry| entry.ok()) {
                        if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                                list.push(value);
                            }
                        }
                    }
                }
                Ok(json!({ "ok": true, "count": list.len(), "assistants": list }))
            }
            "assistant_task" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::invalid("informe `name`"))?;
                let task = args
                    .get("task")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `task`"))?;
                let timeout = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(300);
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_delegate_to": slug(name), "task": task }));
                }
                let assistant = load(name)?;
                let answer = delegate(&assistant, task, timeout)?;
                Ok(json!({ "ok": true, "assistant": slug(name), "answer": answer }))
            }
            "assistant_delete" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_delete": slug(name) }));
                }
                let removed = std::fs::remove_file(path_for(name)).is_ok();
                Ok(json!({ "ok": removed, "removed": removed, "assistant": slug(name) }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> AssistantTool {
    AssistantTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "assistant_create",
        "Cria um assistente permanente com nome, papel e instruções; a Friday pode delegar tarefas a ele.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "role": { "type": "string" },
                "instructions": { "type": "string" }
            }
        }),
    ));
    registry.add(tool(
        "assistant_list",
        "Lista os assistentes permanentes criados pela Friday.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "assistant_task",
        "Delega uma tarefa a um assistente permanente e devolve a resposta dele.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name", "task"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "task": { "type": "string" },
                "timeout_secs": { "type": "integer", "minimum": 30, "maximum": 600, "default": 300 }
            }
        }),
    ));
    registry.add(tool(
        "assistant_delete",
        "Remove um assistente permanente pelo nome.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
}
