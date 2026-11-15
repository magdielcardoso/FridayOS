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

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub developer_instructions: Option<String>,
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
            extra_args: Vec::new(),
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CodexEvent {
    /// Handshake + thread created, ready for turns.
    Online { thread_id: String },
    /// Streamed answer text.
    Delta(String),
    /// Streamed reasoning summary (rendered dimmer in the UI).
    Reasoning(String),
    /// Turn finished (completed, failed, interrupted).
    Done { status: String },
    Error(String),
}

#[derive(Debug)]
enum Command {
    Prompt(String),
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

        let mut session = match Session::connect(child, stdout, stdin, &config, &events).await {
            Ok(session) => session,
            Err(error) => {
                let _ = events.send(CodexEvent::Error(error));
                continue;
            }
        };

        loop {
            // Wait for a prompt, idle.
            let text = loop {
                match commands.recv().await {
                    Some(Command::Prompt(text)) => break text,
                    Some(Command::Cancel) => continue,
                    Some(Command::Shutdown) | None => {
                        session.kill().await;
                        return;
                    }
                }
            };

            // Run the turn, but stay responsive to cancel/shutdown.
            tokio::select! {
                result = session.turn(&text) => {
                    if let Err(error) = result {
                        let _ = events.send(CodexEvent::Error(error));
                        break;
                    }
                }
                command = commands.recv() => {
                    match command {
                        Some(Command::Cancel) => {
                            session.kill().await;
                            let _ = events.send(CodexEvent::Done { status: "interrupted".into() });
                            break;
                        }
                        Some(Command::Shutdown) | None => {
                            session.kill().await;
                            return;
                        }
                        Some(Command::Prompt(_)) => {
                            // A new prompt while a turn is running: drop it, the UI
                            // only allows one turn at a time.
                        }
                    }
                }
            }
        }
    }
}

type Parts = (Child, Lines<BufReader<ChildStdout>>, ChildStdin);

async fn launch(config: &CodexConfig) -> Result<Parts, String> {
    let mut child = tokio::process::Command::new(CODEX_BIN)
        .args(&config.extra_args)
        .envs(config.env.iter().map(|(key, value)| (key, value)))
        .arg("app-server")
        .arg("--stdio")
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "O CLI do Codex não está instalado. Instale com `npm i -g @openai/codex` e faça login."
                    .to_string()
            }
            _ => format!("Não foi possível iniciar o Codex: {e}"),
        })?;

    let stdout = child.stdout.take().ok_or("Codex sem stdout")?;
    let stdin = child.stdin.take().ok_or("Codex sem stdin")?;
    Ok((child, BufReader::new(stdout).lines(), stdin))
}

struct Session {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    stdin: ChildStdin,
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
        stdin: ChildStdin,
        config: &CodexConfig,
        events: &mpsc::UnboundedSender<CodexEvent>,
    ) -> Result<Self, String> {
        let mut session = Self {
            child,
            lines,
            stdin,
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

        let result = session.request("thread/start", params).await?;
        let thread_id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or("Codex não devolveu o id da thread")?
            .to_string();
        session.thread_id = thread_id.clone();
        let _ = session.events.send(CodexEvent::Online { thread_id });
        Ok(session)
    }

    async fn turn(&mut self, text: &str) -> Result<(), String> {
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
        if let Some(effort) = self.turn_effort.clone() {
            params["effort"] = json!(effort);
        }
        let id = self.send_request("turn/start", params).await?;
        let (tx, _rx) = oneshot::channel();
        self.pending.insert(id, tx);

        // Drain notifications until the turn completes.
        loop {
            match self.next_message().await? {
                Some(message) => self.handle(message).await,
                None => return Err("O processo do Codex foi encerrado durante o turno.".into()),
            }
            if self.turn_finished {
                self.turn_finished = false;
                return Ok(());
            }
        }
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
                "turn/completed" => {
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
        let mut line = serde_json::to_vec(message).map_err(|e| e.to_string())?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|e| format!("Falha escrevendo no Codex: {e}"))
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

    let auth = crate::auth::CodexAuth::discover().path().to_path_buf();
    let link = dir.join("auth.json");
    if !link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&auth, &link)?;
        #[cfg(not(unix))]
        std::fs::copy(&auth, &link)?;
    }

    let model = std::env::var("FRIDAY_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let mut config = format!(
        "model = \"{model}\"\n\
         model_reasoning_effort = \"low\"\n\
         model_reasoning_summary = \"none\"\n\
         model_verbosity = \"low\"\n\
         hide_agent_reasoning = true\n\
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
