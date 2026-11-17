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

/// A server-initiated approval left unanswered this long is declined so a
/// forgotten prompt cannot keep the Codex turn waiting forever.
const APPROVAL_TIMEOUT_SECS: u64 = 120;

/// `initialize` / `thread/resume` / `thread/start` are local to the CLI. They
/// should finish in well under a second; 15 s is generous. Failing fast here
/// lets `supervise` retry with backoff instead of hanging behind the 120 s
/// deadline used for ordinary requests.
const HANDSHAKE_TIMEOUT_SECS: u64 = 15;

/// Compact automatically after this many completed turns without a compaction.
const AUTO_COMPACT_TURNS: u32 = 12;
/// ...or once the deltas streamed since the last compaction pass ~24k chars.
const AUTO_COMPACT_CHARS: usize = 24_000;

/// Builds the failure used when a handshake step stalls, so supervise can log a
/// clear reason and apply its backoff instead of spinning forever.
fn handshake_timeout_error(method: &str) -> String {
    format!("tempo esgotado ({HANDSHAKE_TIMEOUT_SECS}s) no handshake do Codex: {method}")
}

/// True when the CLI says a persisted thread id is gone or unusable. Such an id
/// must be forgotten, otherwise every reconnect retries the same dead thread.
fn is_thread_not_found(error: &str) -> bool {
    let lower = error.to_lowercase();
    [
        "not found",
        "não encontrad",
        "nao encontrad",
        "invalid",
        "inválid",
        "unknown thread",
        "does not exist",
        "não existe",
        "nao existe",
        "404",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Snapshot returned by `Codex::debug_state`. Shared by the idle and in-turn
/// command handlers so both produce exactly the same shape for traces.
fn debug_snapshot(thread_id: &str, resume: &Option<String>, turns_since_compact: u32) -> Value {
    json!({
        "thread_id": thread_id,
        "resume": resume,
        "turns_since_compact": turns_since_compact,
    })
}

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
    /// The thread was summarized to reclaim context.
    Compacted,
    /// A tool/command step the UI can show.
    Step {
        id: String,
        label: String,
        state: StepState,
        /// Result/output of the step, when available (command output, tool result).
        detail: Option<String>,
    },
    /// A server-initiated approval/permission request that needs a human
    /// decision. The session does not answer it until `approve`/`deny` arrives.
    ApprovalRequest {
        id: u64,
        method: String,
        summary: String,
        params: Value,
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
    /// Ask the server to summarize the thread and shrink its context.
    Compact,
    /// Drop the current child and connect again (watchdog recovery).
    Reconnect,
    /// Answer a pending server-initiated approval request positively.
    Approve { id: u64 },
    /// Answer a pending server-initiated approval request negatively.
    Deny { id: u64 },
    /// Internal: a pending approval outlived its deadline and must be declined.
    ApprovalTimeout { id: u64 },
    /// Internal: reply with a snapshot of the live session state (for traces).
    Debug { reply: oneshot::Sender<Value> },
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
        // A clone lives inside each Session so a stalled approval can inject a
        // `Command::ApprovalTimeout` back into the very loop that owns it.
        let timeout_cmd_tx = cmd_tx.clone();

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
                runtime.block_on(supervise(
                    config,
                    event_tx.clone(),
                    cmd_rx,
                    timeout_cmd_tx,
                ));
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

    /// Approves a pending `CodexEvent::ApprovalRequest` by id. No-op if the
    /// request was already answered, timed out, or belongs to another session.
    pub fn approve(&self, id: u64) {
        let _ = self.tx.send(Command::Approve { id });
    }

    /// Denies a pending `CodexEvent::ApprovalRequest` by id.
    pub fn deny(&self, id: u64) {
        let _ = self.tx.send(Command::Deny { id });
    }

    /// Summarizes the thread to reclaim context without losing the session.
    pub fn compact(&self) {
        let _ = self.tx.send(Command::Compact);
    }

    /// Forces a clean reconnect; used when a turn wedges.
    pub fn reconnect(&self) {
        let _ = self.tx.send(Command::Reconnect);
    }

    /// Trace-friendly snapshot of the live session state. The worker answers
    /// this command inline in its select loop (also while a turn is running),
    /// so it never waits for a turn to end. Bounded so a UI thread cannot wedge
    /// if the worker is mid-handshake or already gone.
    pub fn debug_state(&self) -> Value {
        let unavailable = |reason: &str| {
            json!({
                "thread_id": Value::Null,
                "resume": Value::Null,
                "turns_since_compact": 0,
                "error": reason,
            })
        };
        let (reply, mut rx) = oneshot::channel();
        if self.tx.send(Command::Debug { reply }).is_err() {
            return unavailable("canal do Codex fechado");
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match rx.try_recv() {
                Ok(value) => return value,
                Err(oneshot::error::TryRecvError::Closed) => {
                    return unavailable("worker do Codex não respondeu");
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return unavailable("worker do Codex ocupado");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
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
    mut config: CodexConfig,
    events: mpsc::UnboundedSender<CodexEvent>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    timeout_cmd_tx: mpsc::UnboundedSender<Command>,
) {
    // Kept in sync with the live session, so a reconnect resumes the same
    // conversation instead of silently starting a new one.
    let mut resume = config.resume_thread.clone();
    let mut failures = 0u32;
    loop {
        config.resume_thread = resume.clone();
        let (child, stdout, stdin) = match launch(&config).await {
            Ok(parts) => parts,
            Err(error) => {
                let _ = events.send(CodexEvent::Error(error));
                return;
            }
        };

        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(writer_task(stdin, rx));

        let mut session = match Session::connect(
            child,
            stdout,
            tx.clone(),
            &config,
            &events,
            timeout_cmd_tx.clone(),
            &mut resume,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                let _ = events.send(CodexEvent::Error(error));
                // Never spin: a failed handshake used to relaunch the CLI in a
                // tight loop, burning a core and spawning processes forever.
                failures = failures.saturating_add(1);
                let delay = failures.min(8) as u64;
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                continue;
            }
        };
        failures = 0;
        // Safety net: connect already tracks the live thread, but keep this so
        // `resume` can never drift from the thread the child actually created.
        resume = Some(session.thread_id.clone());

        let steer_tx = tx.clone();
        let turn_slot = session.turn_id.clone();
        let thread_id = session.thread_id.clone();

        'session: loop {
            // Idle: wait for a prompt. A steer with no turn running becomes one.
            let text = loop {
                match commands.recv().await {
                    Some(Command::Prompt(text)) | Some(Command::Steer(text)) => break text,
                    Some(Command::Cancel) => continue,
                    Some(Command::Approve { id }) => {
                        session.respond_to_approval(id, true).await;
                        continue;
                    }
                    Some(Command::Deny { id }) => {
                        session.respond_to_approval(id, false).await;
                        continue;
                    }
                    Some(Command::ApprovalTimeout { id }) => {
                        session.expire_approval(id).await;
                        continue;
                    }
                    Some(Command::Compact) => {
                        match session.compact_now().await {
                            Ok(()) => {
                                session.reset_compaction_counters();
                                let _ = events.send(CodexEvent::Compacted);
                            }
                            Err(error) => {
                                let _ = events.send(CodexEvent::Error(format!(
                                    "compactação do contexto falhou: {error}"
                                )));
                            }
                        }
                        continue;
                    }
                    Some(Command::Debug { reply }) => {
                        let _ = reply.send(debug_snapshot(
                            &session.thread_id,
                            &resume,
                            session.turns_since_compact,
                        ));
                        continue;
                    }
                    Some(Command::Reconnect) => {
                        session.kill().await;
                        resume = Some(session.thread_id.clone());
                        continue 'session;
                    }
                    Some(Command::Shutdown) | None => {
                        session.kill().await;
                        return;
                    }
                }
            };

            // Discard notifications left over from the previous turn, so a
            // stale `turn/completed` cannot end the new one immediately.
            session.drain().await;

            if let Err(error) = session.start_turn(&text).await {
                let _ = events.send(CodexEvent::Error(error));
                let _ = events.send(CodexEvent::Done {
                    status: "failed".into(),
                });
                break;
            }

            enum TurnEvent {
                Line(Result<Option<String>, String>),
                Cmd(Option<Command>),
            }

            // Only a turn that actually reached its end (completed or failed
            // notification) is counted for auto-compaction. A dead process, a
            // reconnect or a cancel must never trigger a compact request.
            let mut turn_completed = false;

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
                            turn_completed = true;
                            break;
                        }
                    }
                    TurnEvent::Line(Ok(None)) => {
                        let _ = events.send(CodexEvent::Error(
                            "O processo do Codex foi encerrado durante o turno.".into(),
                        ));
                        let _ = events.send(CodexEvent::Done {
                            status: "failed".into(),
                        });
                        break;
                    }
                    TurnEvent::Line(Err(error)) => {
                        let _ = events.send(CodexEvent::Error(error));
                        let _ = events.send(CodexEvent::Done {
                            status: "failed".into(),
                        });
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
                    TurnEvent::Cmd(Some(Command::Reconnect)) => {
                        resume = Some(session.thread_id.clone());
                        session.kill().await;
                        break;
                    }
                    TurnEvent::Cmd(Some(Command::Approve { id })) => {
                        session.respond_to_approval(id, true).await;
                    }
                    TurnEvent::Cmd(Some(Command::Deny { id })) => {
                        session.respond_to_approval(id, false).await;
                    }
                    TurnEvent::Cmd(Some(Command::ApprovalTimeout { id })) => {
                        session.expire_approval(id).await;
                    }
                    TurnEvent::Cmd(Some(Command::Debug { reply })) => {
                        let _ = reply.send(debug_snapshot(
                            &session.thread_id,
                            &resume,
                            session.turns_since_compact,
                        ));
                    }
                    TurnEvent::Cmd(Some(Command::Compact)) => {}
                    TurnEvent::Cmd(Some(Command::Cancel)) => {
                        let turn_id = turn_slot
                            .lock()
                            .ok()
                            .and_then(|slot| slot.clone());
                        if let Some(turn_id) = turn_id {
                            // Fire and forget: waiting for the reply could block
                            // the cancel path for two minutes.
                            let _ = session
                                .send_request(
                                    "turn/interrupt",
                                    json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id
                                    }),
                                )
                                .await;
                        }
                        let _ = events.send(CodexEvent::Done {
                            status: "interrupted".into(),
                        });
                        // Keep the process alive: killing it made the next
                        // message vanish into a dead child.
                        break;
                    }
                    TurnEvent::Cmd(Some(Command::Shutdown)) | TurnEvent::Cmd(None) => {
                        session.kill().await;
                        return;
                    }
                }
            }

            // Auto-compaction happens only between turns, never mid-stream.
            // Counters reset on success, so this cannot cascade or loop.
            if turn_completed {
                session.turns_since_compact = session.turns_since_compact.saturating_add(1);
                if session.turns_since_compact >= AUTO_COMPACT_TURNS
                    || session.prompt_chars_since_compact >= AUTO_COMPACT_CHARS
                {
                    match session.compact_now().await {
                        Ok(()) => {
                            session.reset_compaction_counters();
                            let _ = events.send(CodexEvent::Compacted);
                        }
                        Err(error) => {
                            // Cool down instead of erroring on every following
                            // turn if the server keeps rejecting the compact.
                            session.reset_compaction_counters();
                            let _ = events.send(CodexEvent::Error(format!(
                                "compactação automática falhou: {error}"
                            )));
                        }
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

/// A server-initiated request we surfaced to the UI and are holding open.
#[derive(Debug, Clone)]
struct PendingApproval {
    method: String,
    params: Value,
}

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
    /// Workspace used by `turn/start`. The process cwd is `/` in a bundled app,
    /// which made sandbox and shell resolve in the wrong place.
    cwd: String,
    turn_effort: Option<String>,
    turn_finished: bool,
    in_turn: bool,
    /// Completed turns since the last (manual or automatic) compaction.
    turns_since_compact: u32,
    /// Rough size of streamed deltas since the last compaction, in chars.
    prompt_chars_since_compact: usize,
    seen_deltas: std::collections::HashSet<String>,
    commentary_items: std::collections::HashSet<String>,
    events: mpsc::UnboundedSender<CodexEvent>,
    /// Approval requests waiting for a human decision, by JSON-RPC id.
    approvals: HashMap<u64, PendingApproval>,
    /// Clone of the command channel, used to schedule timeout notifications.
    command_tx: mpsc::UnboundedSender<Command>,
}

impl Session {
    async fn connect(
        child: Child,
        lines: Lines<BufReader<ChildStdout>>,
        tx: mpsc::UnboundedSender<Value>,
        config: &CodexConfig,
        events: &mpsc::UnboundedSender<CodexEvent>,
        command_tx: mpsc::UnboundedSender<Command>,
        // Updated in place so `supervise` always resumes the live thread id.
        resume: &mut Option<String>,
    ) -> Result<Self, String> {
        let mut session = Self {
            child,
            lines,
            tx,
            turn_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            next_id: 0,
            pending: HashMap::new(),
            thread_id: String::new(),
            cwd: config.cwd.clone(),
            turn_effort: config.effort.clone(),
            turn_finished: false,
            in_turn: false,
            turns_since_compact: 0,
            prompt_chars_since_compact: 0,
            seen_deltas: std::collections::HashSet::new(),
            commentary_items: std::collections::HashSet::new(),
            events: events.clone(),
            approvals: HashMap::new(),
            command_tx,
        };

        // Handshake steps get their own short deadline: a wedged CLI must not
        // hold `supervise` hostage for the full 120 s request timeout.
        tokio::time::timeout(
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            session.request(
                "initialize",
                json!({
                    "clientInfo": { "name": "friday", "title": "FridayOS", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": false, "requestAttestation": false }
                }),
            ),
        )
        .await
        .map_err(|_| handshake_timeout_error("initialize"))??;
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
            let mut resume_params = json!({
                "threadId": previous,
                "cwd": config.cwd,
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write"
            });
            if let Some(model) = config.model.as_deref().filter(|m| *m != "default") {
                resume_params["model"] = json!(model);
            }
            match tokio::time::timeout(
                std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
                session.try_request("thread/resume", resume_params),
            )
            .await
            {
                // A timeout is a stall, not a dead thread: fail the handshake so
                // supervise retries the same id with backoff.
                Err(_) => return Err(handshake_timeout_error("thread/resume")),
                Ok(Ok(value)) => {
                    if let Some(id) = value.pointer("/thread/id").and_then(Value::as_str) {
                        resumed = Some(id.to_string());
                        *resume = resumed.clone();
                    } else {
                        // Accepted but no usable id: forget it, start fresh.
                        *resume = None;
                    }
                }
                Ok(Err(error)) => {
                    if is_thread_not_found(&error) {
                        // Persisted id points at a thread that no longer exists.
                        *resume = None;
                    }
                    // Other errors keep `resume` so a later reconnect retries
                    // the same thread. Either way a new thread is made below.
                }
            }
        }

        let thread_id = if let Some(id) = resumed {
            id
        } else {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
                session.request("thread/start", params),
            )
            .await
            .map_err(|_| handshake_timeout_error("thread/start"))??;
            result
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or("Codex não devolveu o id da thread")?
                .to_string()
        };
        // Single source of truth: whatever the CLI just resumed or created is
        // the live thread every later reconnect must resume.
        *resume = Some(thread_id.clone());
        session.thread_id = thread_id.clone();
        let _ = session.events.send(CodexEvent::Online { thread_id });
        Ok(session)
    }

    async fn start_turn(&mut self, text: &str) -> Result<(), String> {
        let mut params = json!({
            "threadId": self.thread_id,
            "input": [{ "type": "text", "text": text, "text_elements": [] }],
            "cwd": self.cwd.clone(),
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
        self.in_turn = true;
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
            // Server-initiated requests carry a non-null id. Notifications may
            // carry `"id": null`, which is not a request and keeps its old path.
            if let Some(id_value) = message.get("id").filter(|id| !id.is_null()) {
                // Friday's own tool server already enforces risk + confirmation,
                // so its elicitation stays auto-accepted. Every other request
                // that needs a decision is handed to the user instead of being
                // silently declined.
                let is_friday_elicitation = method == "mcpServer/elicitation/request"
                    && params
                        .get("serverName")
                        .and_then(Value::as_str)
                        .map(|name| name == "friday")
                        .unwrap_or(false);

                if !is_friday_elicitation {
                    if let Some(id) = id_value.as_u64() {
                        let summary = approval_summary(method, &params);
                        self.approvals.insert(
                            id,
                            PendingApproval {
                                method: method.to_string(),
                                params: params.clone(),
                            },
                        );
                        let _ = self.events.send(CodexEvent::ApprovalRequest {
                            id,
                            method: method.to_string(),
                            summary,
                            params,
                        });

                        // If the user never answers, decline on a timer so a
                        // forgotten prompt cannot wedge the session forever.
                        let command_tx = self.command_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                APPROVAL_TIMEOUT_SECS,
                            ))
                            .await;
                            let _ = command_tx.send(Command::ApprovalTimeout { id });
                        });
                        return;
                    }

                    // A non-numeric id cannot be surfaced as an ApprovalRequest;
                    // send a best-effort decline instead of leaving it hanging.
                    let _ = self.events.send(CodexEvent::Error(format!(
                        "Solicitação de aprovação com id não numérico ({method}); respondi 'decline' automaticamente."
                    )));
                    let reply = json!({ "id": id_value, "result": { "decision": "declined" } });
                    let _ = self.write(&reply).await;
                    return;
                }

                // Historical behaviour for Friday's own MCP tool server.
                let reply =
                    json!({ "id": id_value, "result": { "action": "accept", "content": {} } });
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
                            detail: step_detail(&item),
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
                        self.note_delta_chars(delta.len());
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
                        self.note_delta_chars(delta.len());
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
                            detail: step_detail(&item),
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
                                    self.note_delta_chars(text.len());
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
                    self.in_turn = false;
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
                    // Without a Done the UI stays stuck in PENSANDO forever.
                    if self.in_turn {
                        self.in_turn = false;
                        let _ = self.events.send(CodexEvent::Done {
                            status: "failed".into(),
                        });
                    }
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
                    // A rejected request during a turn used to leave the turn
                    // open forever (no Done, UI stuck in PENSANDO).
                    if self.in_turn {
                        self.in_turn = false;
                        self.turn_finished = true;
                        // This is the `turn/start` (or another in-turn request)
                        // error path: always close the turn for the UI.
                        let _ = self.events.send(CodexEvent::Done {
                            status: "failed".into(),
                        });
                    }
                }
                let _ = sender.send(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// Both manual and automatic compaction funnel through here, so the
    /// counters are always reset at the same point `CodexEvent::Compacted`
    /// leaves the session.
    fn reset_compaction_counters(&mut self) {
        self.turns_since_compact = 0;
        self.prompt_chars_since_compact = 0;
    }

    /// Cheap heuristic for "the prompt is getting big": sum every streamed
    /// delta (answer and reasoning) since the last compaction. Capped so a
    /// single huge answer cannot overflow the counter.
    fn note_delta_chars(&mut self, chars: usize) {
        self.prompt_chars_since_compact = self
            .prompt_chars_since_compact
            .saturating_add(chars)
            .min(AUTO_COMPACT_CHARS.saturating_mul(4));
    }

    /// Bounded compact request; used by both the manual and automatic paths so
    /// a wedged server cannot stall the command loop for 120 s.
    async fn compact_now(&mut self) -> Result<(), String> {
        tokio::time::timeout(
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            self.try_request(
                "thread/compact/start",
                json!({ "threadId": self.thread_id.clone() }),
            ),
        )
        .await
        .map_err(|_| "tempo esgotado ao compactar o contexto".to_string())??;
        Ok(())
    }

    /// Discards lines already sitting in the pipe (late notifications from a
    /// previous/interrupted turn) so they cannot end the next turn instantly.
    async fn drain(&mut self) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(30)),
                self.lines.next_line(),
            )
            .await
            {
                Ok(Ok(Some(_))) => continue,
                _ => break,
            }
        }
    }

    async fn next_message(&mut self) -> Result<Option<Value>, String> {
        match self.lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    return Ok(Some(Value::Null));
                }
                // A stray non-JSON line must not look like EOF: that false
                // "process died" triggered the relaunch loop.
                Ok(Some(serde_json::from_str(&line).unwrap_or(Value::Null)))
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
            let next = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                self.next_message(),
            )
            .await
            .map_err(|_| "tempo esgotado esperando o Codex".to_string())??;
            match next {
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
            let next = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                self.next_message(),
            )
            .await
            .map_err(|_| "tempo esgotado esperando o Codex".to_string())??;
            match next {
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

    /// Answers a surfaced approval request and removes it from the pending map.
    /// The reply is queued to the writer task, so this never blocks on the CLI.
    async fn respond_to_approval(&mut self, id: u64, approved: bool) {
        let Some(approval) = self.approvals.remove(&id) else {
            // Already answered, timed out, or from a previous session.
            return;
        };
        let (result, warning) = approval_reply(&approval.method, &approval.params, approved);
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.write(&reply).await;
        if let Some(warning) = warning {
            let _ = self.events.send(CodexEvent::Error(warning));
        }
    }

    /// A timed-out approval is declined with the same schema as an explicit
    /// deny. Removing it first makes a late user click a harmless no-op.
    async fn expire_approval(&mut self, id: u64) {
        let Some(approval) = self.approvals.remove(&id) else {
            return;
        };
        let (result, _) = approval_reply(&approval.method, &approval.params, false);
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.write(&reply).await;
        let _ = self.events.send(CodexEvent::Error(format!(
            "Aprovação #{id} expirou após {APPROVAL_TIMEOUT_SECS}s e foi recusada automaticamente."
        )));
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Resolves the Codex CLI path and whether a subscription login is present.
pub fn status() -> (bool, bool) {
    // Use the same resolution as `launch`: a GUI app has a minimal PATH, so
    // `which` reported "CLI não encontrado" while chat worked fine.
    (codex_binary().is_some(), CodexAuth::discover().exists())
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
            // Compact MCP routes everything through `exec`; the real action is
            // in the arguments, so surface it instead of the meta-tool.
            let inner = item
                .get("arguments")
                .and_then(|args| args.get("name").or_else(|| args.get("tool")))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty());
            let tool = inner.unwrap_or(tool);
            let tool = tool
                .strip_prefix("friday__")
                .or_else(|| tool.strip_prefix("friday."))
                .unwrap_or(tool);
            let server = item.get("server").and_then(Value::as_str).unwrap_or("");
            if server.is_empty() || server == "friday" {
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

/// Extrai um resumo do resultado de um passo, para inspeção no HUD.
fn step_detail(item: &Value) -> Option<String> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let text = match kind {
        "commandExecution" => {
            let output = item
                .get("aggregatedOutput")
                .or_else(|| item.get("aggregated_output"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let code = item
                .get("exitCode")
                .or_else(|| item.get("exit_code"))
                .and_then(Value::as_i64);
            if output.trim().is_empty() {
                code.map(|code| format!("sem saída · código {code}"))
            } else {
                Some(match code {
                    Some(code) => format!("código {code}\n{}", truncate(output, 2000)),
                    None => truncate(output, 2000),
                })
            }
        }
        "mcpToolCall" | "dynamicToolCall" => {
            let result = item
                .get("result")
                .or_else(|| item.get("error"))
                .filter(|value| !value.is_null());
            result.map(|value| {
                let raw = value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string());
                truncate(&raw, 2000)
            })
        }
        "fileChange" | "patchApply" => item
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .filter_map(|change| {
                        let path = change.get("path").and_then(Value::as_str)?;
                        let kind = change.get("kind").and_then(Value::as_str).unwrap_or("edit");
                        Some(format!("{kind} {path}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|text| !text.is_empty()),
        "webSearch" => item
            .get("results")
            .or_else(|| item.get("result"))
            .filter(|value| !value.is_null())
            .map(|value| truncate(&value.to_string(), 1600)),
        _ => None,
    };
    text.filter(|text| !text.trim().is_empty())
}

/// Builds the JSON-RPC `result` for a stored approval decision. The optional
/// second value is a UI warning for methods whose schema we had to guess.
fn approval_reply(method: &str, params: &Value, approved: bool) -> (Value, Option<String>) {
    if method == "mcpServer/elicitation/request" {
        let action = if approved { "accept" } else { "decline" };
        return (json!({ "action": action, "content": {} }), None);
    }
    if method.contains("requestApproval") {
        let decision = if approved { "approved" } else { "declined" };
        return (json!({ "decision": decision }), None);
    }
    if method.contains("requestPermissions") {
        let granted = if approved {
            requested_permissions(params)
        } else {
            Vec::new()
        };
        return (json!({ "permissions": granted, "scope": "turn" }), None);
    }
    (
        json!({}),
        Some(format!(
            "Método de aprovação desconhecido ({method}); enviei uma resposta best-effort. Se o Codex não a aceitar, a solicitação pode falhar."
        )),
    )
}

/// Best-effort extraction of permission names, so an approval can echo back
/// what the request asked for. Covers arrays, objects of flags and bare strings.
fn requested_permissions(params: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut roots: Vec<&Value> = Vec::new();
    for key in ["permissions", "requestedPermissions", "permission"] {
        if let Some(value) = params.get(key) {
            roots.push(value);
        }
    }
    if roots.is_empty() {
        roots.push(params);
    }
    for value in roots {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => {
                for item in items {
                    if let Some(text) = item.as_str() {
                        out.push(text.to_string());
                    } else if let Some(map) = item.as_object() {
                        for key in ["permission", "name", "id", "type"] {
                            if let Some(text) = map.get(key).and_then(Value::as_str) {
                                out.push(text.to_string());
                                break;
                            }
                        }
                    }
                }
            }
            Value::Object(map) => {
                for (key, flag) in map {
                    if flag.as_bool().unwrap_or(false) || flag.is_null() {
                        out.push(key.clone());
                    } else if let Some(text) = flag.as_str() {
                        out.push(format!("{key}={text}"));
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Short human-facing summary of a pending approval, capped at ~160 chars.
fn approval_summary(method: &str, params: &Value) -> String {
    let label = if method == "mcpServer/elicitation/request" {
        "aprovação de ferramenta MCP"
    } else if method.contains("requestApproval") {
        "aprovação necessária"
    } else if method.contains("requestPermissions") {
        "permissão solicitada"
    } else {
        "solicitação do servidor"
    };

    let mut details: Vec<String> = Vec::new();
    if let Some(server) = find_string(params, &["serverName", "server", "server_name"]) {
        details.push(format!("servidor {server}"));
    }
    if let Some(tool) = find_string(params, &["tool", "toolName"]) {
        details.push(format!("ferramenta {tool}"));
    }
    if let Some(command) = find_string(params, &["command", "cmd", "argv"]) {
        details.push(format!("comando {}", truncate(command, 80)));
    }
    if let Some(permission) = find_string(
        params,
        &["permission", "permissions", "permissionProfile", "requestedPermissions"],
    ) {
        details.push(format!("permissão {}", truncate(permission, 60)));
    }
    if let Some(reason) = find_string(params, &["reason", "message", "description", "explanation"]) {
        details.push(format!("motivo {}", truncate(reason, 60)));
    }

    let summary = if details.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {}", details.join(" · "))
    };
    truncate(&summary, 160)
}

/// Recursively finds the first non-empty string stored under any of `keys`.
/// Codex nests approval details in `params`, `info`, arrays and so on.
fn find_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(nested) = value.get(key) {
            if let Some(text) = nested.as_str() {
                if !text.trim().is_empty() {
                    return Some(text.trim());
                }
            }
            if let Some(found) = find_string(nested, keys) {
                return Some(found);
            }
        }
    }
    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(text) = item.as_str() {
                if !text.trim().is_empty() {
                    return Some(text.trim());
                }
            }
            if let Some(found) = find_string(item, keys) {
                return Some(found);
            }
        }
    }
    None
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
