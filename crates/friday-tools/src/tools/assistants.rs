//! Permanent assistants Friday can create by name and delegate tasks to.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::artifact;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn harness_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/harness.json")
}

/// Maximum allowed orchestration depth: env override, then
/// `harness.json.subagent_depth`, then 2.
fn max_subagent_depth() -> u64 {
    if let Ok(raw) = std::env::var("FRIDAY_SUBAGENT_DEPTH") {
        if let Ok(value) = raw.trim().parse::<u64>() {
            return value.min(8);
        }
    }
    std::fs::read_to_string(harness_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("subagent_depth")
                .and_then(Value::as_u64)
                .or_else(|| {
                    value
                        .get("harness")
                        .and_then(|nested| nested.get("subagent_depth"))
                        .and_then(Value::as_u64)
                })
        })
        .unwrap_or(2)
        .min(8)
}

fn active_path(name: &str) -> PathBuf {
    dir().join(format!(".{}.active.json", slug(name)))
}

fn write_active(name: &str, value: &Value) {
    let path = active_path(name);
    let _ = std::fs::create_dir_all(dir());
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        let _ = std::fs::write(path, bytes);
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max {
            break;
        }
        out.push(ch);
    }
    out
}

/// Normalise a worker's raw output into the structured result contract:
/// `{ok, answer, evidence, verification, findings}`. Plain text is wrapped as
/// `answer` with `verification: "unknown"`.
fn structure_answer(raw: &str, budget_chars: Option<usize>) -> Value {
    let mut result = if let Some(object) = serde_json::from_str::<Value>(raw)
        .ok()
        .filter(|value| value.is_object())
    {
        let answer = object
            .get("answer")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(&object).unwrap_or_default());
        let evidence = object
            .get("evidence")
            .cloned()
            .filter(|value| value.is_array())
            .unwrap_or_else(|| json!([]));
        let findings = object
            .get("findings")
            .cloned()
            .filter(|value| value.is_array())
            .unwrap_or_else(|| json!([]));
        let verification = match object.get("verification").and_then(Value::as_str) {
            Some("pass") => "pass",
            Some("fail") => "fail",
            _ => "unknown",
        };
        let ok = object.get("ok").and_then(Value::as_bool).unwrap_or(true);
        json!({
            "ok": ok,
            "answer": answer,
            "evidence": evidence,
            "verification": verification,
            "findings": findings,
        })
    } else {
        json!({
            "ok": true,
            "answer": raw.trim().to_string(),
            "evidence": [],
            "verification": "unknown",
            "findings": [],
        })
    };
    if let Some(max) = budget_chars {
        let replacement = result
            .get("answer")
            .and_then(Value::as_str)
            .filter(|text| text.chars().count() > max)
            .map(|text| truncate_chars(text, max));
        if let Some(text) = replacement {
            result["answer"] = json!(text);
        }
    }
    result
}

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

fn pid_path(name: &str) -> PathBuf {
    dir().join(format!("{}.pid", slug(name)))
}

fn thread_path(name: &str) -> PathBuf {
    dir().join(format!("{}.thread", slug(name)))
}

fn running_pid(name: &str) -> Option<u32> {
    let raw = std::fs::read_to_string(pid_path(name)).ok()?;
    let pid = raw.trim().parse::<u32>().ok()?;
    let alive = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    alive.then_some(pid)
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

fn delegate(assistant: &Value, task: &str, timeout_secs: u64, meta: &Value) -> Result<String, ToolError> {
    let binary = codex_binary().ok_or_else(|| ToolError::failed("CLI do Codex não encontrado"))?;
    let name = assistant.get("name").and_then(|v| v.as_str()).unwrap_or("assistente");
    let role = assistant.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let instructions = assistant.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
    let mut prompt = format!(
        "Você é {name}, um assistente permanente criado pela Friday.\n\
         Papel: {role}\n\
         Instruções: {instructions}\n\n\
         Tarefa delegada pela Friday: {task}\n\n\
         Responda de forma objetiva e completa, em português."
    );
    if let Some(budget) = meta.get("budget_chars").and_then(Value::as_u64) {
        if budget > 0 {
            prompt.push_str(&format!("\nResponda em no máximo {budget} caracteres."));
        }
    }

    if let Some(pid) = running_pid(name) {
        return Err(ToolError::failed(format!(
            "`{name}` já está executando (pid {pid}); cancele antes de delegar outra tarefa"
        )));
    }
    // The HUD watches this file to show what the subagent is doing live.
    let live = live_path(name);
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(&live, "");
    live_append(&live, "task", task);
    live_append(&live, "status", "iniciando o Codex…");
    if meta.as_object().is_some_and(|map| !map.is_empty()) {
        live_append(&live, "meta", &serde_json::to_string(meta).unwrap_or_default());
    }

    // One answer file per assistant: sharing a pid-only path made concurrent
    // delegations return each other's answers.
    let out = std::env::temp_dir().join(format!(
        "friday-assistant-{}-{}.txt",
        std::process::id(),
        slug(name)
    ));
    let _ = std::fs::remove_file(&out);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let previous_thread = std::fs::read_to_string(thread_path(name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut command = Command::new(binary);
    if let Some(thread) = previous_thread.as_deref() {
        command.args(["exec", "resume", thread]);
        live_append(&live, "status", "retomando a sessão anterior…");
    } else {
        command.arg("exec");
    }
    command
        .args(["--json", "--skip-git-repo-check", "-o"])
        .arg(&out)
        .args([
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "sandbox_mode=\"workspace-write\"",
        ])
        .arg(&prompt)
        .current_dir(&home)
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
    let pid = child.id();
    let _ = std::fs::write(pid_path(name), pid.to_string());

    let thread_file = thread_path(name);
    let mut reader = child.stdout.take().map(|stdout| {
        let live = live.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if event.get("type").and_then(Value::as_str) == Some("thread.started") {
                    if let Some(id) = event.get("thread_id").and_then(Value::as_str) {
                        let _ = std::fs::write(&thread_file, id);
                    }
                }
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
                let _ = std::fs::remove_file(pid_path(name));
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
                // Do not leave the child running invisibly.
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(pid_path(name));
                live_append(&live, "error", &error.to_string());
                return Err(ToolError::failed(error.to_string()));
            }
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(pid_path(name));
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
                        // Skip `.pid`, `.thread`, `.live.jsonl` and the hidden
                        // `.active.json` records: a bare pid parsed as JSON
                        // became a phantom assistant.
                        if entry.file_name().to_string_lossy().starts_with('.') {
                            continue;
                        }
                        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                            continue;
                        }
                        if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                            if let Ok(mut value) = serde_json::from_str::<Value>(&raw) {
                                let key = value.get("slug").and_then(Value::as_str).unwrap_or("");
                                if !key.is_empty() {
                                    if let Ok(active_raw) = std::fs::read_to_string(active_path(key)) {
                                        if let Ok(active) = serde_json::from_str::<Value>(&active_raw) {
                                            value["run_id"] =
                                                active.get("run_id").cloned().unwrap_or(Value::Null);
                                            value["depth"] =
                                                active.get("depth").cloned().unwrap_or(json!(0));
                                            value["status"] = active
                                                .get("status")
                                                .cloned()
                                                .unwrap_or(json!("unknown"));
                                            if let Some(verification) =
                                                active.get("verification").and_then(Value::as_str)
                                            {
                                                value["verification"] = json!(verification);
                                            } else if let Some(run_id) =
                                                active.get("run_id").and_then(Value::as_str)
                                            {
                                                if let Some((status, _)) =
                                                    artifact::verification_status(run_id)
                                                {
                                                    value["verification"] = json!(status);
                                                }
                                            }
                                        }
                                    }
                                }
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
                let run_id = args
                    .get("run_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let depth = args.get("depth").and_then(Value::as_i64).unwrap_or(0);
                if depth < 0 {
                    return Err(ToolError::invalid("`depth` não pode ser negativo"));
                }
                let max_depth = max_subagent_depth() as i64;
                if depth > max_depth {
                    return Err(ToolError::invalid(format!(
                        "`depth` {depth} excede o limite de {max_depth} (harness.json → subagent_depth); delegação mais profunda não é permitida"
                    )));
                }
                let goal = args
                    .get("goal")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let condition = args
                    .get("condition")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let budget_chars = args
                    .get("budget_chars")
                    .and_then(Value::as_u64)
                    .filter(|&value| value > 0);

                let meta = json!({
                    "run_id": run_id,
                    "depth": depth,
                    "goal": goal,
                    "condition": condition,
                    "budget_chars": budget_chars,
                });

                if ctx.dry_run {
                    return Ok(json!({
                        "dry_run": true,
                        "would_delegate_to": slug(name),
                        "task": task,
                        "run_id": run_id,
                        "depth": depth,
                        "goal": goal,
                        "condition": condition,
                        "budget_chars": budget_chars,
                        "max_depth": max_depth,
                    }));
                }

                let assistant = load(name)?;
                write_active(
                    name,
                    &json!({
                        "run_id": run_id,
                        "depth": depth,
                        "goal": goal,
                        "condition": condition,
                        "status": "running",
                        "started": now_secs(),
                    }),
                );

                let raw = match delegate(&assistant, task, timeout, &meta) {
                    Ok(raw) => raw,
                    Err(error) => {
                        write_active(
                            name,
                            &json!({
                                "run_id": run_id,
                                "depth": depth,
                                "goal": goal,
                                "condition": condition,
                                "status": "error",
                                "error": error.to_string(),
                                "finished": now_secs(),
                            }),
                        );
                        return Err(error);
                    }
                };
                let mut result = structure_answer(&raw, budget_chars.map(|value| value as usize));
                result["assistant"] = json!(slug(name));
                result["run_id"] = json!(run_id);
                result["depth"] = json!(depth);
                result["goal"] = json!(goal);
                result["condition"] = json!(condition);
                result["budget_chars"] = json!(budget_chars);

                let verification = result
                    .get("verification")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                write_active(
                    name,
                    &json!({
                        "run_id": run_id,
                        "depth": depth,
                        "goal": goal,
                        "condition": condition,
                        "status": "done",
                        "verification": verification,
                        "finished": now_secs(),
                    }),
                );

                if let Some(run_id) = run_id.as_deref() {
                    if let Some(run) = artifact::load_run(run_id) {
                        let preview = result
                            .get("answer")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let _ = run.append_finding(&format!(
                            "worker `{assistant}` (depth {depth}) respondeu: {summary}",
                            assistant = slug(name),
                            summary = truncate_chars(preview, 400)
                        ));
                        let _ = run.record_tool("assistant_task", &meta);
                        let _ = run.set_verification(&verification, "worker concluído");
                    }
                }

                Ok(result)
            }
            "assistant_cancel" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_cancel": slug(name) }));
                }
                let Some(pid) = running_pid(name) else {
                    let _ = std::fs::remove_file(pid_path(name));
                    return Ok(json!({
                        "ok": true,
                        "cancelled": false,
                        "assistant": slug(name),
                        "note": "não havia tarefa em execução"
                    }));
                };
                if !pid_is_assistant(pid) {
                    // The process died and the pid was reused; killing it would
                    // hit an unrelated program.
                    let _ = std::fs::remove_file(pid_path(name));
                    return Ok(json!({
                        "ok": true,
                        "cancelled": false,
                        "assistant": slug(name),
                        "note": "tarefa já terminou (pid obsoleto)"
                    }));
                }
                let _ = Command::new("/bin/kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
                // Codex may leave children behind; take them down with it.
                let _ = Command::new("/usr/bin/pkill")
                    .args(["-TERM", "-P", &pid.to_string()])
                    .status();
                std::thread::sleep(Duration::from_millis(400));
                if running_pid(name).is_some() {
                    let _ = Command::new("/bin/kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
                let _ = std::fs::remove_file(pid_path(name));
                live_append(&live_path(name), "error", "cancelado pelo usuário");
                Ok(json!({
                    "ok": true,
                    "cancelled": true,
                    "assistant": slug(name),
                    "pid": pid,
                }))
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

/// True when `pid` still looks like a Codex child, never an arbitrary process.
fn pid_is_assistant(pid: u32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let line = String::from_utf8_lossy(&output.stdout).to_lowercase();
    line.contains("codex")
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
                "timeout_secs": { "type": "integer", "minimum": 30, "maximum": 600, "default": 300 },
                "run_id": { "type": "string", "description": "Id de uma execução (run) para registrar achados e verificação." },
                "depth": { "type": "integer", "minimum": 0, "default": 0, "description": "Profundidade de delegação; limitada por harness.json → subagent_depth (padrão 2)." },
                "goal": { "type": "string", "description": "Objetivo deste worker dentro da run." },
                "condition": { "type": "string", "description": "Critério pelo qual o worker é considerado concluído." },
                "budget_chars": { "type": "integer", "minimum": 1, "description": "Limite de caracteres da resposta final." }
            }
        }),
    ));
    registry.add(tool(
        "assistant_cancel",
        "Interrompe a tarefa em execução de um assistente permanente.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
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
