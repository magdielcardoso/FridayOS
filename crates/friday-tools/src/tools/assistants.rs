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

/// Public path of the live activity feed the HUD tails while an assistant runs.
pub fn live_path(name: &str) -> PathBuf {
    dir().join(format!("{}.live.jsonl", slug(name)))
}

fn live_append(path: &PathBuf, kind: &str, text: &str) {
    use std::io::Write;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let record = json!({
        "ts": now_secs(),
        "kind": kind,
        "text": text.chars().take(4000).collect::<String>(),
    });
    let _ = writeln!(file, "{record}");
}

/// Turns one `codex exec --json` event into a short human-readable line.
fn summarize_event(event: &Value) -> Option<(String, String)> {
    let ty = event.get("type")?.as_str()?;
    match ty {
        "thread.started" => Some(("status".into(), "sessão iniciada".into())),
        "turn.started" => Some(("status".into(), "pensando…".into())),
        "error" => Some((
            "error".into(),
            event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("erro")
                .to_string(),
        )),
        "item.started" | "item.completed" => {
            let item = event.get("item")?;
            let completed = ty == "item.completed";
            let kind = item.get("type").and_then(Value::as_str)?;
            match kind {
                "command_execution" => {
                    let command = item
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("comando")
                        .to_string();
                    if !completed {
                        return Some(("command".into(), command));
                    }
                    let code = item.get("exit_code").and_then(Value::as_i64);
                    let output = item
                        .get("aggregated_output")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let preview: String = output.chars().take(700).collect();
                    Some((
                        if code.unwrap_or(0) == 0 { "output" } else { "error" }.into(),
                        format!("{command} → saiu {code:?}\n{preview}"),
                    ))
                }
                "file_change" => {
                    let changes = item
                        .get("changes")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(|change| {
                                    let path = change.get("path").and_then(Value::as_str)?;
                                    let kind = change.get("kind").and_then(Value::as_str).unwrap_or("edita");
                                    Some(format!("{kind} {path}"))
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    Some((
                        if completed { "file" } else { "status" }.into(),
                        if changes.is_empty() {
                            if completed { "arquivos atualizados".into() } else { "editando arquivos…".into() }
                        } else {
                            changes
                        },
                    ))
                }
                "mcp_tool_call" => {
                    let server = item.get("server").and_then(Value::as_str).unwrap_or("?");
                    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("ferramenta");
                    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                    Some((
                        "tool".into(),
                        format!("{server}::{tool} {status}").trim().to_string(),
                    ))
                }
                "agent_message" => {
                    if completed {
                        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                        (!text.trim().is_empty()).then(|| ("message".into(), text.to_string()))
                    } else {
                        None
                    }
                }
                "reasoning" => {
                    let text = item
                        .get("text")
                        .or_else(|| item.get("summary").and_then(|s| s.as_array()).and_then(|a| a.first()))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    (!text.trim().is_empty() && completed).then(|| ("think".into(), text.to_string()))
                }
                other => completed.then(|| ("status".into(), format!("{other} concluído"))),
            }
        }
        "turn.completed" => Some(("status".into(), "turno concluído".into())),
        _ => None,
    }
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

    // The HUD watches this file to show what the subagent is doing live.
    let live = live_path(name);
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(&live, "");
    live_append(&live, "task", task);
    live_append(&live, "status", "iniciando o Codex…");

    let out = std::env::temp_dir().join(format!("friday-assistant-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut command = Command::new(binary);
    command
        .args(["exec", "--json", "--skip-git-repo-check", "-C", &home, "-o"])
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
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        command.env("CODEX_HOME", codex_home);
    }

    let mut child = command
        .spawn()
        .map_err(|error| {
            live_append(&live, "error", &format!("codex exec: {error}"));
            ToolError::failed(format!("codex exec: {error}"))
        })?;

    let mut reader = child.stdout.take().map(|stdout| {
        let live = live.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some((kind, text)) = summarize_event(&event) {
                    live_append(&live, &kind, &text);
                }
            }
        })
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(30, 600));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(handle) = reader.take() {
                    let _ = handle.join();
                }
                let answer = std::fs::read_to_string(&out).unwrap_or_default();
                let _ = std::fs::remove_file(&out);
                if answer.trim().is_empty() {
                    live_append(&live, "error", &format!("terminou sem resposta ({status})"));
                    return Err(ToolError::failed(format!(
                        "o assistente terminou sem resposta ({status})"
                    )));
                }
                live_append(&live, "done", "tarefa concluída");
                return Ok(answer.trim().to_string());
            }
            Ok(None) => {}
            Err(error) => {
                live_append(&live, "error", &error.to_string());
                return Err(ToolError::failed(error.to_string()));
            }
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            live_append(&live, "error", "tempo esgotado");
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
