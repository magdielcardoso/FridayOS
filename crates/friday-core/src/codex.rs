//! Chat transport.
//!
//! Friday does not speak the private ChatGPT backend protocol directly. It
//! drives the Codex CLI the same way the official clients do:
//!
//! ```text
//! codex app-server --stdio   (JSON-RPC 2.0, newline delimited)
//! ```
//!
//! The CLI owns OAuth refresh, headers and the backend wire format, so the
//! subscription keeps working as Codex evolves. We only need `initialize`,
//! `thread/start`, `turn/start` and a handful of notifications.

use std::collections::HashMap;
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::auth::CodexAuth;

const CODEX_BIN: &str = "codex";

fn configured_reasoning_effort() -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let path = home.join("Library/Application Support/FridayOS/reasoning.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    match value.get("effort").and_then(Value::as_str)? {
        effort @ ("low" | "medium" | "high" | "xhigh" | "max") => Some(effort.to_string()),
        _ => None,
    }
}

/// GUI-launched apps inherit a minimal PATH (`/usr/bin:/bin:...`), so `codex`
/// installed by nvm/homebrew is invisible. Resolve it once through the user's
/// login shell, and reuse that same PATH for the child process.
fn login_path() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let fallback = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string();
        let output = std::process::Command::new("/bin/zsh")
            .args(["-lc", "printf %s \"$PATH\""])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let path = String::from_utf8_lossy(&output.stdout).to_string();
                if path.trim().is_empty() {
                    fallback
                } else {
                    path
                }
            }
            _ => fallback,
        }
    })
    .clone()
}

fn codex_binary() -> Option<std::path::PathBuf> {
    static RESOLVED: std::sync::OnceLock<Option<std::path::PathBuf>> =
        std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            if let Ok(explicit) = std::env::var("FRIDAY_CODEX_BIN") {
                let path = std::path::PathBuf::from(explicit);
                if path.is_file() {
                    return Some(path);
                }
            }

            if let Ok(output) = std::process::Command::new("/bin/zsh")
                .args(["-lc", "command -v codex"])
                .output()
            {
                if output.status.success() {
                    let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !found.is_empty() {
                        let path = std::path::PathBuf::from(found);
                        if path.is_file() {
                            return Some(path);
                        }
                    }
                }
            }

            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            let mut candidates = vec![
                std::path::PathBuf::from("/opt/homebrew/bin/codex"),
                std::path::PathBuf::from("/usr/local/bin/codex"),
                home.join(".local/bin/codex"),
            ];
            // nvm: pick the highest installed Node version that has codex.
            let nvm = home.join(".nvm/versions/node");
            if let Ok(entries) = std::fs::read_dir(&nvm) {
                let mut versions: Vec<_> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .collect();
                versions.sort();
                versions.reverse();
                for version in versions {
                    candidates.push(version.join("bin/codex"));
                }
            }
            candidates.into_iter().find(|path| path.is_file())
        })
        .clone()
}

/// Absolute path to the resolved Codex CLI (for diagnostics).
pub fn codex_path() -> Option<std::path::PathBuf> {
    codex_binary()
}

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub developer_instructions: Option<String>,
    /// Thread to resume if it still exists in the local Codex store.
    pub resume_thread: Option<String>,
    /// Extra CLI args, e.g. `-c` overrides.
    pub extra_args: Vec<String>,
    /// Environment for the Codex child (mainly `CODEX_HOME`).
    pub env: Vec<(String, String)>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into()),
            model: None,
            effort: None,
            developer_instructions: None,
            resume_thread: None,
            extra_args: Vec::new(),
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub enum CodexEvent {
    /// Handshake + thread created, ready for turns.
    Online { thread_id: String },
    /// Streamed answer text.
    Delta(String),
    /// Streamed reasoning summary (rendered dimmer in the UI).
    Reasoning(String),
    /// A tool/command step the UI can show.
    Step {
        id: String,
        label: String,
        state: StepState,
    },
    /// Turn finished (completed, failed, interrupted).
    Done { status: String },
    Error(String),
}

#[derive(Debug)]
enum Command {
    Prompt(String),
    /// Guidance for the turn that is already running.
    Steer(String),
    Cancel,
    Shutdown,
}

pub struct Codex {
    events: Option<mpsc::UnboundedReceiver<CodexEvent>>,
    tx: mpsc::UnboundedSender<Command>,
    thread: std::thread::JoinHandle<()>,
}

impl Codex {
    /// Spawns a dedicated runtime thread that owns the Codex child process.
    pub fn spawn(config: CodexConfig) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let thread = std::thread::Builder::new()
            .name("friday-codex".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = event_tx.send(CodexEvent::Error(format!(
                            "Não foi possível iniciar o runtime do Codex: {error}"
                        )));
                        return;
                    }
                };
                runtime.block_on(supervise(config, event_tx.clone(), cmd_rx));
            })
            .expect("spawn friday-codex thread");

        Self {
            events: Some(event_rx),
            tx: cmd_tx,
            thread,
        }
    }

    /// Takes the event stream. Call once, right after spawning.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<CodexEvent>> {
        self.events.take()
    }

    pub fn prompt(&self, text: impl Into<String>) {
        let _ = self.tx.send(Command::Prompt(text.into()));
    }

    /// Sends guidance to the running turn instead of starting a new one.
    pub fn steer(&self, text: impl Into<String>) {
        let _ = self.tx.send(Command::Steer(text.into()));
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(Command::Cancel);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl Drop for Codex {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        // The thread is detached: killing the process tree must not block the UI.
        let _ = &self.thread;
    }
}

/// Restarts the CLI if it dies; keeps the same thread id when possible.
async fn supervise(
    config: CodexConfig,
    events: mpsc::UnboundedSender<CodexEvent>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    loop {
        let (child, stdout, stdin) = match launch(&config).await {
            Ok(parts) => parts,
            Err(error) => {
                let _ = events.send(CodexEvent::Error(error));
                return;
            }
        };

        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(writer_task(stdin, rx));

        let mut session = match Session::connect(child, stdout, tx.clone(), &config, &events).await {
            Ok(session) => session,
            Err(error) => {
                let _ = events.send(CodexEvent::Error(error));
                continue;
            }
        };

        let steer_tx = tx.clone();
        let turn_slot = session.turn_id.clone();
        let thread_id = session.thread_id.clone();

        loop {
            // Idle: wait for a prompt. A steer with no turn running becomes one.
            let text = loop {
                match commands.recv().await {
                    Some(Command::Prompt(text)) | Some(Command::Steer(text)) => break text,
                    Some(Command::Cancel) => continue,
                    Some(Command::Shutdown) | None => {
                        session.kill().await;
                        return;
                    }
                }
            };

            if let Err(error) = session.start_turn(&text).await {
                let _ = events.send(CodexEvent::Error(error));
                break;
            }

            enum TurnEvent {
                Line(Result<Option<String>, String>),
                Cmd(Option<Command>),
            }

            loop {
                let event = {
                    let lines = &mut session.lines;
                    tokio::select! {
                        line = lines.next_line() => TurnEvent::Line(line.map_err(|e| e.to_string())),
                        command = commands.recv() => TurnEvent::Cmd(command),
                    }
                };

                match event {
                    TurnEvent::Line(Ok(Some(line))) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Ok(message) = serde_json::from_str::<Value>(&line) {
                            session.handle(message).await;
                        }
                        if session.turn_finished {
                            session.turn_finished = false;
                            break;
                        }
                    }
                    TurnEvent::Line(Ok(None)) => {
                        let _ = events.send(CodexEvent::Error(
                            "O processo do Codex foi encerrado durante o turno.".into(),
                        ));
                        break;
                    }
                    TurnEvent::Line(Err(error)) => {
                        let _ = events.send(CodexEvent::Error(error));
                        break;
                    }
                    TurnEvent::Cmd(Some(Command::Steer(text)))
                    | TurnEvent::Cmd(Some(Command::Prompt(text))) => {
                        // Guidance for the live turn: keeps the context intact.
                        let turn_id = turn_slot
                            .lock()
                            .ok()
                            .and_then(|slot| slot.clone());
                        if let Some(turn_id) = turn_id {
                            let _ = steer_tx.send(json!({
                                "jsonrpc": "2.0",
                                "id": 0,
                                "method": "turn/steer",
                                "params": {
                                    "threadId": thread_id,
                                    "expectedTurnId": turn_id,
                                    "input": [{ "type": "text", "text": text, "text_elements": [] }]
                                }
                            }));
                        }
                    }
                    TurnEvent::Cmd(Some(Command::Cancel)) => {
                        session.kill().await;
                        let _ = events.send(CodexEvent::Done {
                            status: "interrupted".into(),
                        });
                        break;
                    }
                    TurnEvent::Cmd(Some(Command::Shutdown)) | TurnEvent::Cmd(None) => {
                        session.kill().await;
                        return;
                    }
                }
            }
        }
    }
}

async fn writer_task(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<Value>) {
    while let Some(message) = rx.recv().await {
        let Ok(mut line) = serde_json::to_vec(&message) else {
            continue;
        };
        line.push(b'\n');
        if stdin.write_all(&line).await.is_err() {
            break;
        }
    }
}

type Parts = (Child, Lines<BufReader<ChildStdout>>, ChildStdin);

async fn launch(config: &CodexConfig) -> Result<Parts, String> {
    let binary = codex_binary().ok_or_else(|| {
        "O CLI do Codex não foi encontrado. Instale com `npm i -g @openai/codex`, faça login uma vez e reabra a Friday.".to_string()
    })?;

    // Also expose node's directory (nvm/homebrew) so the codex shim can find it.
    let mut path = login_path();
    if let Some(dir) = binary.parent() {
        path = format!("{}:{}", dir.display(), path);
    }

    let mut child = tokio::process::Command::new(&binary)
        .args(&config.extra_args)
        .env("PATH", path)
        .envs(config.env.iter().map(|(key, value)| (key, value)))
        .arg("app-server")
        .arg("--stdio")
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Não foi possível iniciar o Codex ({}): {e}", binary.display()))?;

    let stdout = child.stdout.take().ok_or("Codex sem stdout")?;
    let stdin = child.stdin.take().ok_or("Codex sem stdin")?;
    Ok((child, BufReader::new(stdout).lines(), stdin))
}

struct Session {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    tx: mpsc::UnboundedSender<Value>,
    turn_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    next_id: u64,
    pending: HashMap<u64, oneshot::Sender<Value>>,
    thread_id: String,
    turn_effort: Option<String>,
    turn_finished: bool,
    seen_deltas: std::collections::HashSet<String>,
    commentary_items: std::collections::HashSet<String>,
    events: mpsc::UnboundedSender<CodexEvent>,
}

impl Session {
    async fn connect(
        child: Child,
        lines: Lines<BufReader<ChildStdout>>,
        tx: mpsc::UnboundedSender<Value>,
        config: &CodexConfig,
        events: &mpsc::UnboundedSender<CodexEvent>,
    ) -> Result<Self, String> {
        let mut session = Self {
            child,
            lines,
            tx,
            turn_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            next_id: 0,
            pending: HashMap::new(),
            thread_id: String::new(),
            turn_effort: config.effort.clone(),
            turn_finished: false,
            seen_deltas: std::collections::HashSet::new(),
            commentary_items: std::collections::HashSet::new(),
            events: events.clone(),
        };

        session
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "friday", "title": "FridayOS", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": false, "requestAttestation": false }
                }),
            )
            .await?;
        session.notify("initialized", json!({})).await?;

        let mut params = json!({
            "cwd": config.cwd,
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "ephemeral": false
        });
        if let Some(model) = config.model.as_deref().filter(|m| *m != "default") {
            params["model"] = json!(model);
        }
        if let Some(instructions) = config
            .developer_instructions
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            params["developerInstructions"] = json!(instructions);
        }

        let mut resumed = None;
        if let Some(previous) = config.resume_thread.as_deref() {
            let mut resume = json!({
                "threadId": previous,
                "cwd": config.cwd,
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write"
            });
            if let Some(model) = config.model.as_deref().filter(|m| *m != "default") {
                resume["model"] = json!(model);
            }
            if let Ok(value) = session.try_request("thread/resume", resume).await {
                if let Some(id) = value.pointer("/thread/id").and_then(Value::as_str) {
                    resumed = Some(id.to_string());
                }
            }
        }

        let thread_id = if let Some(id) = resumed {
            id
        } else {
            let result = session.request("thread/start", params).await?;
            result
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or("Codex não devolveu o id da thread")?
                .to_string()
        };
        session.thread_id = thread_id.clone();
        let _ = session.events.send(CodexEvent::Online { thread_id });
        Ok(session)
    }

    async fn start_turn(&mut self, text: &str) -> Result<(), String> {
        let mut params = json!({
            "threadId": self.thread_id,
            "input": [{ "type": "text", "text": text, "text_elements": [] }],
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into()),
            "approvalPolicy": "on-request",
            "sandboxPolicy": {
                "type": "workspaceWrite",
                "writableRoots": [],
                "networkAccess": false,
                "excludeTmpdirEnvVar": true,
                "excludeSlashTmp": true
            }
        });
        if let Some(effort) = configured_reasoning_effort().or_else(|| self.turn_effort.clone()) {
            params["effort"] = json!(effort);
        }
        self.turn_finished = false;
        if let Ok(mut slot) = self.turn_id.lock() {
            *slot = None;
        }
        let id = self.send_request("turn/start", params).await?;
        let (tx, _rx) = oneshot::channel();
        self.pending.insert(id, tx);
        Ok(())
    }

    async fn handle(&mut self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);

        if let Some(method) = method {
            if std::env::var_os("FRIDAY_CODEX_DEBUG").is_some() {
                eprintln!("[codex] {method} {}", message);
            }
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            // Server-initiated requests have an id: decline politely for now.
            if let Some(id) = message.get("id") {
                let reply = if method == "mcpServer/elicitation/request" {
                    // Codex asks the host to approve MCP tool calls. Friday's own
                    // tool server already enforces risk + confirmation, so only
                    // its calls are auto-accepted here.
                    let is_friday = params
                        .get("serverName")
                        .and_then(Value::as_str)
                        .map(|name| name == "friday")
                        .unwrap_or(false);
                    let action = if is_friday { "accept" } else { "decline" };
                    json!({ "id": id, "result": { "action": action, "content": {} } })
                } else if method.contains("requestApproval") {
                    json!({ "id": id, "result": { "decision": "decline" } })
                } else if method.contains("requestPermissions") {
                    json!({ "id": id, "result": { "permissions": {}, "scope": "turn" } })
                } else {
                    json!({ "id": id, "result": {} })
                };
                let _ = self.write(&reply).await;
                return;
            }

            match method {
                "item/started" => {
                    let item = params.get("item").cloned().unwrap_or(Value::Null);
                    if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && item.get("phase").and_then(Value::as_str) == Some("commentary")
                    {
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            self.commentary_items.insert(id.to_string());
                        }
                    }
                    if let Some((id, label)) = step_label(&item) {
                        let _ = self.events.send(CodexEvent::Step {
                            id,
                            label,
                            state: StepState::Started,
                        });
                    }
                }
                "item/agentMessage/delta" => {
                    if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                        let item = params
                            .get("itemId")
                            .and_then(Value::as_str)
                            .unwrap_or("default")
                            .to_string();
                        let commentary = params.get("phase").and_then(Value::as_str)
                            == Some("commentary")
                            || self.commentary_items.contains(&item);
                        self.seen_deltas.insert(item);
                        let event = if commentary {
                            CodexEvent::Reasoning(delta.to_string())
                        } else {
                            CodexEvent::Delta(delta.to_string())
                        };
                        let _ = self.events.send(event);
                    }
                }
                "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                    if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                        let _ = self.events.send(CodexEvent::Reasoning(delta.to_string()));
                    }
                }
                "item/completed" => {
                    let item = params.get("item").cloned().unwrap_or(Value::Null);
                    if let Some((id, label)) = step_label(&item) {
                        let failed = item
                            .get("status")
                            .and_then(Value::as_str)
                            .map(|status| matches!(status, "failed" | "declined" | "cancelled"))
                            .unwrap_or(false)
                            || item.get("error").map(|e| !e.is_null()).unwrap_or(false);
                        let _ = self.events.send(CodexEvent::Step {
                            id,
                            label,
                            state: if failed {
                                StepState::Failed
                            } else {
                                StepState::Completed
                            },
                        });
                    }
                    if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                        // Fallback only when this item never streamed deltas.
                        let item_id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("default")
                            .to_string();
                        if !self.seen_deltas.contains(&item_id) {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    let _ = self.events.send(CodexEvent::Delta(text.to_string()));
                                }
                            }
                        }
                    }
                }
                "turn/started" => {
                    if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) {
                        if let Ok(mut slot) = self.turn_id.lock() {
                            *slot = Some(id.to_string());
                        }
                    }
                }
                "turn/completed" => {
                    if let Ok(mut slot) = self.turn_id.lock() {
                        *slot = None;
                    }
                    let status = params
                        .pointer("/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .to_string();
                    let _ = self.events.send(CodexEvent::Done { status });
                    self.turn_finished = true;
                }
                "error" => {
                    let message = params
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Erro no servidor Codex.");
                    let _ = self.events.send(CodexEvent::Error(message.to_string()));
                    self.turn_finished = true;
                }
                _ => {}
            }
            return;
        }

        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some(sender) = self.pending.remove(&id) {
                if let Some(error) = message.get("error") {
                    let _ = self.events.send(CodexEvent::Error(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Erro no Codex")
                            .to_string(),
                    ));
                }
                let _ = sender.send(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    async fn next_message(&mut self) -> Result<Option<Value>, String> {
        match self.lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    return Ok(Some(Value::Null));
                }
                Ok(serde_json::from_str(&line).ok())
            }
            Ok(None) => Ok(None),
            Err(error) => Err(format!("Falha lendo o Codex: {error}")),
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send_request(method, params).await?;
        let (tx, mut rx) = oneshot::channel();
        self.pending.insert(id, tx);

        loop {
            match self.next_message().await? {
                Some(message) => self.handle(message).await,
                None => return Err("O processo do Codex foi encerrado.".into()),
            }
            // handle() resolves pending requests; poll the channel.
            if let Ok(result) = rx.try_recv() {
                return Ok(result);
            }
        }
    }

    /// Like `request`, but returns the server error instead of emitting it.
    async fn try_request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send_request(method, params).await?;
        let (tx, mut rx) = oneshot::channel();
        self.pending.insert(id, tx);
        loop {
            match self.next_message().await? {
                Some(message) => {
                    if message.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = message.get("error") {
                            return Err(error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("erro")
                                .to_string());
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    self.handle(message).await;
                }
                None => return Err("processo encerrado".into()),
            }
        }
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<u64, String> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write(&message).await?;
        Ok(id)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&message).await
    }

    async fn write(&mut self, message: &Value) -> Result<(), String> {
        self.tx
            .send(message.clone())
            .map_err(|_| "O processo do Codex foi encerrado.".to_string())
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Resolves the Codex CLI path and whether a subscription login is present.
pub fn status() -> (bool, bool) {
    let bin = which(CODEX_BIN);
    (bin.is_some(), CodexAuth::discover().exists())
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Creates a clean `CODEX_HOME` for Friday: no user plugins, hooks or unrelated
/// MCP servers, just the model settings and the Friday tool server. This is the
/// difference between a ~5 s and a ~3 s first token on a stock Codex install.
pub fn prepare_isolated_home(mcp_command: Option<&std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let dir = std::path::PathBuf::from(home)
        .join("Library/Application Support/FridayOS/codex");
    std::fs::create_dir_all(&dir)?;

    let auth = crate::auth::active_auth_path();
    let link = dir.join("auth.json");
    // Always refresh the link: the active account may have changed.
    if link.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&link);
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&auth, &link)?;
    #[cfg(not(unix))]
    std::fs::copy(&auth, &link)?;

    let model = std::env::var("FRIDAY_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let mut config = format!(
        "model = \"{model}\"\n\
         model_reasoning_effort = \"low\"\n\
         model_reasoning_summary = \"none\"\n\
         model_verbosity = \"low\"\n\
         hide_agent_reasoning = true\n\
         service_tier = \"priority\"\n\
         approval_policy = \"on-request\"\n\
         sandbox_mode = \"workspace-write\"\n\
         \n"
    );
    if let Some(command) = mcp_command {
        config.push_str(&format!(
            "[mcp_servers.friday]\ncommand = \"{}\"\nstartup_timeout_sec = 20\n",
            command.display()
        ));
    }
    std::fs::write(dir.join("config.toml"), config)?;
    Ok(dir)
}

/// Turns a Codex item into a short human step for the UI.
fn step_label(item: &Value) -> Option<(String, String)> {
    let id = item.get("id").and_then(Value::as_str)?.to_string();
    let kind = item.get("type").and_then(Value::as_str)?;
    let detail = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    };
    let label = match kind {
        "commandExecution" => {
            let command = detail("command");
            let first = command.lines().next().unwrap_or("").trim().to_string();
            if first.is_empty() {
                "executando comando".to_string()
            } else {
                format!("executando · {}", truncate(&first, 60))
            }
        }
        "mcpToolCall" | "dynamicToolCall" => {
            let tool = item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("ferramenta");
            let server = item.get("server").and_then(Value::as_str).unwrap_or("");
            if server.is_empty() {
                format!("ferramenta · {tool}")
            } else {
                format!("{server} · {tool}")
            }
        }
        "fileChange" | "patchApply" => "alterando arquivos".to_string(),
        "webSearch" => format!("pesquisando · {}", truncate(&detail("query"), 50)),
        "webFetch" => format!("lendo · {}", truncate(&detail("url"), 50)),
        "imageGeneration" => "gerando imagem".to_string(),
        "reasoning" => "raciocinando".to_string(),
        "plan" => "planejando".to_string(),
        _ => return None,
    };
    Some((id, label))
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push('…');
        out
    }
}
