//! FridayOS — a JARVIS-like shell.
//!
//! Black substrate, red phosphor, monospace grid. No light theme.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use friday_core::codex::{Codex, CodexConfig, CodexEvent, StepState};
use gpui::prelude::*;
use gpui::{
    canvas, div, point, px, rgb, size, App, Application, Bounds, Context, FocusHandle,
    KeyDownEvent, PathBuilder, Render, ScrollHandle, SharedString, Window, WindowBounds,
    WindowOptions,
};

mod voice;

const BG: u32 = 0x050505;
const RED: u32 = 0xff2f2f;
const RED_SOFT: u32 = 0xff6b6b;
const RED_DIM: u32 = 0x991f1f;
const RED_FAINT: u32 = 0x3b0d0d;
const BONE: u32 = 0xd9caca;

const BANNER: [&str; 5] = [
    " ██████  ██████  ██  ██████   █████  ██   ██",
    " ██      ██   ██ ██  ██   ██ ██   ██  ██ ██ ",
    " █████   ██████  ██  ██   ██ ███████   ███  ",
    " ██      ██   ██ ██  ██   ██ ██   ██  ██ ██ ",
    " ██      ██   ██ ██  ██████   ██   ██ ██   ██",
];

const PERSONA_BASE: &str = "\
You are Friday — the private assistant of this macOS machine, built in FridayOS.
Speak Brazilian Portuguese by default. You are Jarvis: composed, precise, a
little dry, never theatrical. Address the user as 'senhor' when it fits.
You ARE the system: you see the screen, control apps, type, click, run shell
commands, manage files, network, calendar and voice. Never say you lack tools
or that something is not wired. If an action fails, report the real error and
the next step.
Your answers are spoken out loud by the app: 1 to 3 short sentences, no lists,
no markdown, no tables, no emoji. Expand only when asked.

Do not emit commentary or thinking out loud. Answer directly: the first token
you produce is spoken immediately.
TOOLS
A tool server named `friday` is connected and exposes the tools below. Call
them through the MCP server; the tool names are exactly as listed.
Risk policy, non-negotiable:
- read: use freely.
- reversible: act, then mention it in one sentence.
- sensitive: ask the user out loud for permission, wait for the next turn, and
  only then retry the same call with \"__confirm\": true.
- critical: same as sensitive, and say clearly what will be touched.
Autonomy levels are yours to propose and configure with `friday__autonomy_set`.
- locked: every state-changing action asks first.
- standard (default): sensitive and critical actions ask first.
- full: use any tool freely; only DESTRUCTIVE actions ask first
  (fs_trash, fs_move, fs_write, process_kill, power_sleep, keychain_set,
  shell_run, applescript_run, jxa_run, app_quit, wifi_join, codex_login,
  codex_account_switch, autonomy_set).
When the user says something like `you have total access`, confirm out loud
(pergunte em voz alta se confirma o acesso total) and, on the next turn, call
`friday__autonomy_set` with level `full` and `__confirm: true`. Never raise
your own autonomy without that explicit grant.
If voice fails because of the ChatGPT hourly limit, offer to authenticate
another Codex account: ask for permission, then call `friday__codex_login`
(the user confirms in the browser). It activates the new account by default and
the app reconnects by itself. `friday__codex_accounts` lists what exists.
Never claim a result before seeing the tool output. Never invent paths, files
or command output. If the user asks for something possible, do it instead of
explaining how they could do it.";

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Connecting,
    Online,
    Thinking,
    Interrupted,
    Offline,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    User,
    Friday,
    System,
}

struct Activity {
    id: String,
    label: String,
    state: StepState,
}

struct Line {
    role: Role,
    text: String,
}

struct FridayApp {
    codex: Option<Codex>,
    messages: Vec<Line>,
    input: String,
    stream: String,
    reasoning: String,
    status: Status,
    thread: Option<String>,
    tools: usize,
    mic: Option<voice::Mic>,
    voice_phase: voice::VoicePhase,
    level: f32,
    speak_cancel: Option<Arc<AtomicBool>>,
    speech_tx: Option<std::sync::mpsc::Sender<String>>,
    speech_pending: String,
    speech_batch: String,
    speech_sent_first: bool,
    tts_stream: bool,
    speaking_since: Option<std::time::Instant>,
    wake: bool,
    wake_flag: Option<Arc<AtomicBool>>,
    account: Option<String>,
    tasks_started: bool,
    blink: bool,
    tick: u64,
    activity: Vec<Activity>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    booted_at: std::time::Instant,
}

impl FridayApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let mut app = Self {
            codex: None,
            messages: vec![Line {
                role: Role::System,
                text: "núcleo em espera · f2 falar · f3 wake word".into(),
            }],
            input: String::new(),
            stream: String::new(),
            reasoning: String::new(),
            status: Status::Connecting,
            thread: None,
            tools: friday_tools::Registry::with_defaults().specs().len(),
            mic: None,
            voice_phase: voice::VoicePhase::Idle,
            level: 0.0,
            speak_cancel: None,
            speech_tx: None,
            speech_pending: String::new(),
            speech_batch: String::new(),
            speech_sent_first: false,
            tts_stream: std::env::var("FRIDAY_TTS_MODE")
                .map(|mode| mode == "stream")
                .unwrap_or(false),
            speaking_since: None,
            wake: false,
            wake_flag: None,
            account: friday_core::auth::active_account(),
            tasks_started: false,
            blink: true,
            tick: 0,
            activity: Vec::new(),
            focus,
            scroll: ScrollHandle::new(),
            booted_at: std::time::Instant::now(),
        };
        app.connect(cx);
        app
    }

    fn start_listen(&mut self, cx: &mut Context<Self>) {
        if self.voice_phase == voice::VoicePhase::Speaking {
            self.interrupt(cx);
        }
        if self.mic.is_none() {
            match voice::Mic::start() {
                Ok(mic) => {
                    self.mic = Some(mic);
                    self.spawn_mic_poll(cx);
                }
                Err(error) => {
                    self.messages.push(Line {
                        role: Role::System,
                        text: format!("microfone indisponível: {error}"),
                    });
                    cx.notify();
                    return;
                }
            }
        }
        if let Some(mic) = self.mic.as_ref() {
            mic.set_speaking(false);
        }
        self.voice_phase = voice::VoicePhase::Listening;
        cx.notify();
    }

    fn stop_listening(&mut self, cx: &mut Context<Self>) {
        self.stop_speech_now();
        if let Some(mut mic) = self.mic.take() {
            mic.stop();
        }
        self.voice_phase = voice::VoicePhase::Idle;
        self.level = 0.0;
        self.messages.push(Line {
            role: Role::System,
            text: "microfone fechado".into(),
        });
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn spawn_mic_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let events = this.update(cx, |app, _| {
                app.mic
                    .as_mut()
                    .map(voice::Mic::poll)
                    .unwrap_or_default()
            });
            let Ok(events) = events else { break };
            let alive = this
                .update(cx, |app, _| app.mic.is_some())
                .unwrap_or(false);
            if !alive {
                break;
            }
            if !events.is_empty() {
                let _ = this.update(cx, |app, cx| {
                    for event in events {
                        app.on_mic(event, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn on_mic(&mut self, event: voice::MicEvent, cx: &mut Context<Self>) {
        match event {
            voice::MicEvent::SpeechStarted => {
                log("mic: fala detectada");
                self.voice_phase = voice::VoicePhase::Listening;
                cx.notify();
            }
            voice::MicEvent::Level(level) => {
                self.level = level;
                cx.notify();
            }
            voice::MicEvent::Utterance { path } => {
                log(format!("mic: utterance {}", path.display()));
                // Never start a second turn on top of the running one: drop the
                // capture and keep the current answer going.
                if self.status == Status::Thinking
                    || self.voice_phase == voice::VoicePhase::Speaking
                {
                    self.voice_phase = voice::VoicePhase::Listening;
                    cx.notify();
                    return;
                }
                self.level = 0.0;
                self.voice_phase = voice::VoicePhase::Transcribing;
                cx.notify();
                Self::blocking(
                    cx,
                    move || voice::transcribe_wav(&path),
                    |app, result, cx| {
                        log(format!("stt: {:?}", result.as_ref().map(|t| t.chars().take(80).collect::<String>())));
                        match result {
                            Ok(text) if !text.trim().is_empty() => {
                                app.messages.push(Line {
                                    role: Role::User,
                                    text: text.clone(),
                                });
                                app.input.clear();
                                app.stream.clear();
                                app.status = Status::Thinking;
                                app.voice_phase = voice::VoicePhase::Idle;
                                if let Some(codex) = app.codex.as_ref() {
                                    codex.prompt(text);
                                }
                                app.scroll.scroll_to_bottom();
                            }
                            Ok(_) => {
                                app.voice_phase = voice::VoicePhase::Listening;
                            }
                            Err(error) => {
                                app.voice_phase = voice::VoicePhase::Idle;
                                app.messages.push(Line {
                                    role: Role::System,
                                    text: format!("não entendi: {error}"),
                                });
                            }
                        }
                        cx.notify();
                    },
                );
            }
            voice::MicEvent::BargeIn => {
                if self.voice_phase == voice::VoicePhase::Speaking
                    || self.status == Status::Thinking
                {
                    self.interrupt(cx);
                    self.voice_phase = voice::VoicePhase::Listening;
                }
            }
            voice::MicEvent::Error(error) => {
                log(format!("mic: erro {error}"));
                self.messages.push(Line {
                    role: Role::System,
                    text: format!("áudio: {error}"),
                });
                self.voice_phase = voice::VoicePhase::Idle;
                cx.notify();
            }
        }
    }

    fn begin_speech(&mut self, cx: &mut Context<Self>) {
        self.stop_speech_now();
        let cancel = Arc::new(AtomicBool::new(false));
        self.speak_cancel = Some(cancel.clone());
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        self.speech_tx = Some(tx);
        self.speech_pending.clear();
        self.speech_batch.clear();
        self.speech_sent_first = false;
        self.voice_phase = voice::VoicePhase::Speaking;
        self.speaking_since = Some(std::time::Instant::now());
        log("speech: começa");
        if let Some(mic) = self.mic.as_ref() {
            mic.set_speaking(true);
        }
        cx.notify();

        let (done_tx, mut done_rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
        std::thread::spawn(move || {
            let result = voice::speak_stream(rx, &cancel);
            let _ = done_tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            if let Some(result) = done_rx.recv().await {
                let _ = this.update(cx, |app, cx| {
                    log("speech: termina");
                    if let Err(error) = result {
                        log(format!("voz: {error}"));
                        app.messages.push(Line {
                            role: Role::System,
                            text: format!("voz indisponível: {error}"),
                        });
                    }
                    app.speaking_since = None;
                    if let Some(mic) = app.mic.as_ref() {
                        mic.set_speaking(false);
                    }
                    app.speak_cancel = None;
                    app.speech_tx = None;
                    app.voice_phase = if app.mic.is_some() {
                        voice::VoicePhase::Listening
                    } else {
                        voice::VoicePhase::Idle
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Streams finished sentences to the speaker while the model is still talking.
    fn feed_speech(&mut self, delta: &str, cx: &mut Context<Self>) {
        self.speech_pending.push_str(delta);
        if !self.tts_stream {
            // Single-request mode: one TTS call per answer keeps us inside
            // ChatGPT's hourly read-aloud quota.
            return;
        }
        if self.speech_tx.is_none() {
            self.begin_speech(cx);
        }
        let sentences = take_sentences(&mut self.speech_pending);
        if let Some(tx) = self.speech_tx.as_ref() {
            for sentence in sentences {
                // One request starts the voice immediately; everything else is
                // batched to respect ChatGPT's hourly TTS request limit.
                if !self.speech_sent_first {
                    self.speech_sent_first = true;
                    let _ = tx.send(sentence);
                } else {
                    self.speech_batch.push_str(&sentence);
                    self.speech_batch.push(' ');
                }
            }
        }
    }

    fn end_speech(&mut self, cx: &mut Context<Self>) {
        if !self.tts_stream {
            let text = self.speech_pending.trim().to_string();
            self.speech_pending.clear();
            if text.is_empty() {
                return;
            }
            self.begin_speech(cx);
            if let Some(tx) = self.speech_tx.as_ref() {
                let _ = tx.send(text);
            }
            self.speech_tx = None;
            return;
        }

        let rest = self.speech_pending.trim().to_string();
        if !rest.is_empty() {
            if !self.speech_sent_first {
                self.speech_sent_first = true;
                if let Some(tx) = self.speech_tx.as_ref() {
                    let _ = tx.send(rest);
                }
            } else {
                self.speech_batch.push_str(&rest);
            }
        }
        let batch = self.speech_batch.trim().to_string();
        if !batch.is_empty() {
            if let Some(tx) = self.speech_tx.as_ref() {
                let _ = tx.send(batch);
            }
        }
        self.speech_batch.clear();
        self.speech_pending.clear();
        self.speech_tx = None; // closes the channel once queued audio is done
        cx.notify();
    }

    fn stop_speech_now(&mut self) {
        self.speaking_since = None;
        if let Some(cancel) = self.speak_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.speech_tx = None;
        self.speech_pending.clear();
        self.speech_batch.clear();
        self.speech_sent_first = false;
        voice::stop_speaking();
        if let Some(mic) = self.mic.as_ref() {
            mic.set_speaking(false);
        }
    }

    fn toggle_wake(&mut self, cx: &mut Context<Self>) {
        if self.wake {
            self.wake = false;
            if let Some(flag) = self.wake_flag.take() {
                flag.store(false, Ordering::Relaxed);
            }
            self.messages.push(Line {
                role: Role::System,
                text: "wake word desligada".into(),
            });
            cx.notify();
            return;
        }

        self.wake = true;
        let flag = Arc::new(AtomicBool::new(true));
        self.wake_flag = Some(flag.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
        std::thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                match voice::wake_once("friday", 6) {
                    Ok(true) => {
                        let _ = tx.send(Ok(()));
                        return;
                    }
                    Ok(false) => continue,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        return;
                    }
                }
            }
        });
        self.messages.push(Line {
            role: Role::System,
            text: "wake word ligada: diga \"Friday\"".into(),
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            if let Some(result) = rx.recv().await {
                let _ = this.update(cx, |app, cx| {
                    app.wake = false;
                    app.wake_flag = None;
                    match result {
                        Ok(()) => app.start_listen(cx),
                        Err(error) => {
                            app.messages.push(Line {
                                role: Role::System,
                                text: format!("wake word: {error}"),
                            });
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.stop_speech_now();
        if self.status == Status::Thinking {
            self.cancel_turn(cx);
        }
        if let Some(mic) = self.mic.as_ref() {
            mic.set_speaking(false);
        }
        self.voice_phase = if self.mic.is_some() {
            voice::VoicePhase::Listening
        } else {
            voice::VoicePhase::Idle
        };
        cx.notify();
    }

    /// Runs blocking work on a plain thread and folds the result back on GPUI.
    fn blocking<T: Send + 'static>(
        cx: &mut Context<Self>,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        cx.spawn(async move |this, cx| {
            if let Some(value) = rx.recv().await {
                let _ = this.update(cx, |app, cx| done(app, value, cx));
            }
        })
        .detach();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let mcp = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("friday-mcp")))
            .filter(|path| path.is_file());

        // Isolated CODEX_HOME: Friday must not inherit the user's plugins,
        // 30 s stop-hooks or unrelated MCP servers.
        let env = match friday_core::codex::prepare_isolated_home(mcp.as_deref()) {
            Ok(home) => vec![(
                "CODEX_HOME".to_string(),
                home.display().to_string(),
            )],
            Err(_) => Vec::new(),
        };

        // Bundled apps launch with cwd "/", which is a terrible Codex workspace.
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let config = CodexConfig {
            cwd,
            model: None,
            effort: Some("low".into()),
            developer_instructions: Some(persona()),
            extra_args: Vec::new(),
            env,
        };

        let mut codex = Codex::spawn(config);
        if let Some(mut events) = codex.take_events() {
            cx.spawn(async move |this, cx| {
                while let Some(event) = events.recv().await {
                    let _ = this.update(cx, |app, cx| app.on_codex(event, cx));
                }
            })
            .detach();
        }
        self.codex = Some(codex);

        if !self.tasks_started {
        self.tasks_started = true;
        // Caret / presence blink.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let _ = this.update(cx, |app, cx| {
                app.tick = app.tick.wrapping_add(1);
                app.blink = (app.tick / 12) % 2 == 0;
                cx.notify();
            });
        })
        .detach();

        // Report (and optionally force) the audio output, so "she does not
        // speak" is never a mystery again.
        Self::blocking(
            cx,
            || {
                let current = friday_tools::swift::run("audio", &["current"]).ok();
                if let Ok(device) = std::env::var("FRIDAY_OUTPUT_DEVICE") {
                    let _ = friday_tools::swift::run("audio", &["set", &device]);
                    return friday_tools::swift::run("audio", &["current"]).ok();
                }
                current
            },
            |app, raw, cx| {
                let value = raw
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok())
                    .unwrap_or(serde_json::Value::Null);
                let name = value
                    .get("name")
                    .and_then(|name| name.as_str())
                    .unwrap_or("desconhecida")
                    .to_string();
                let jack = value.get("jack").and_then(|jack| jack.as_bool());
                let detail = match jack {
                    Some(false) => format!("saída de áudio: {name} · FONE DESCONECTADO"),
                    Some(true) => format!("saída de áudio: {name} · fone conectado"),
                    None => format!("saída de áudio: {name}"),
                };
                log(format!("áudio: {detail}"));
                app.messages.push(Line {
                    role: Role::System,
                    text: detail,
                });
                cx.notify();
            },
        );

        // Watchdog: never stay "speaking" forever because of a stuck TTS.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs(2))
                .await;
            let _ = this.update(cx, |app, cx| {
                if app.voice_phase == voice::VoicePhase::Speaking {
                    let stale = app
                        .speaking_since
                        .map(|since| since.elapsed() > Duration::from_secs(12))
                        .unwrap_or(true);
                    if stale {
                        log("watchdog: fala travada, resetando");
                        app.stop_speech_now();
                        app.voice_phase = voice::VoicePhase::Listening;
                        cx.notify();
                    }
                }
            });
        })
        .detach();

        // Watch for account switches made by the agent's account tools.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs(2))
                .await;
            let current = friday_core::auth::active_account();
            let changed = this
                .update(cx, |app, _| {
                    if app.account != current {
                        app.account = current.clone();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if changed {
                let _ = this.update(cx, |app, cx| {
                    app.messages.push(Line {
                        role: Role::System,
                        text: format!(
                            "conta Codex ativa: {} — reconectando",
                            app.account.clone().unwrap_or_else(|| "padrão".into())
                        ),
                    });
                    app.connect(cx);
                });
            }
        })
        .detach();
        }

        self.status = Status::Connecting;
    }

    fn on_codex(&mut self, event: CodexEvent, cx: &mut Context<Self>) {
        match event {
            CodexEvent::Online { thread_id } => {
                self.status = Status::Online;
                self.thread = Some(thread_id);
                self.stream.clear();
                self.reasoning.clear();
            }
            CodexEvent::Delta(delta) => {
                self.status = Status::Thinking;
                self.stream.push_str(&delta);
                self.feed_speech(&delta, cx);
                self.scroll.scroll_to_bottom();
            }
            CodexEvent::Reasoning(delta) => {
                self.reasoning.push_str(&delta);
                if self.activity.last().map(|a| a.label.as_str()) != Some("raciocinando") {
                    self.push_activity("raciocinando", StepState::Started);
                }
            }
            CodexEvent::Step { id, label, state } => {
                if let Some(existing) = self.activity.iter_mut().find(|item| item.id == id) {
                    existing.label = label;
                    existing.state = state;
                } else {
                    self.activity.push(Activity { id, label, state });
                }
                if self.activity.len() > 40 {
                    self.activity.remove(0);
                }
            }
            CodexEvent::Done { status } => {
                let mut spoken = None;
                if !self.stream.trim().is_empty() {
                    let text = std::mem::take(&mut self.stream);
                    spoken = Some(text.clone());
                    self.messages.push(Line {
                        role: Role::Friday,
                        text,
                    });
                } else if status != "completed" {
                    self.messages.push(Line {
                        role: Role::System,
                        text: format!("turno encerrado: {status}"),
                    });
                }
                self.reasoning.clear();
                self.status = if status == "interrupted" {
                    Status::Interrupted
                } else {
                    Status::Online
                };
                self.scroll.scroll_to_bottom();
                if status == "interrupted" {
                    self.stop_speech_now();
                } else {
                    self.end_speech(cx);
                }
                let _ = spoken;
            }
            CodexEvent::Error(error) => {
                self.messages.push(Line {
                    role: Role::System,
                    text: error,
                });
                self.status = Status::Offline;
                self.scroll.scroll_to_bottom();
            }
        }
        cx.notify();
    }

    fn push_activity(&mut self, label: impl Into<String>, state: StepState) {
        let label = label.into();
        self.activity.push(Activity {
            id: format!("ui-{}", self.tick),
            label,
            state,
        });
        if self.activity.len() > 40 {
            self.activity.remove(0);
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.status == Status::Thinking {
            return;
        }
        self.messages.push(Line {
            role: Role::User,
            text: text.clone(),
        });
        self.input.clear();
        self.stream.clear();
        self.reasoning.clear();
        self.status = Status::Thinking;
        self.activity.clear();
        self.push_activity("enviado ao código", StepState::Completed);

        match self.codex.as_ref() {
            Some(codex) => codex.prompt(text),
            None => {
                self.messages.push(Line {
                    role: Role::System,
                    text: "o núcleo não está conectado".into(),
                });
                self.status = Status::Offline;
            }
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        if self.status != Status::Thinking {
            return;
        }
        if let Some(codex) = self.codex.as_ref() {
            codex.cancel();
        }
        self.status = Status::Interrupted;
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.platform {
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" => self.submit(cx),
            "backspace" => {
                self.input.pop();
                cx.notify();
            }
            "escape" => {
                self.interrupt(cx);
            }
            "f2" => {
                if self.mic.is_some() && self.voice_phase != voice::VoicePhase::Idle {
                    self.stop_listening(cx);
                } else {
                    self.start_listen(cx);
                }
            }
            "f3" => self.toggle_wake(cx),
            "space" => {
                if !modifiers.control {
                    self.input.push(' ');
                    cx.notify();
                }
            }
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref() {
                    if !modifiers.control {
                        self.input.push_str(text);
                        cx.notify();
                    }
                }
            }
        }
    }

    fn status_line(&self) -> (&'static str, u32) {
        match self.status {
            Status::Connecting => ("CONECTANDO AO CODEX", RED_DIM),
            Status::Online => ("CODEX ONLINE", RED),
            Status::Thinking => ("PENSANDO", RED_SOFT),
            Status::Interrupted => ("TURNO INTERROMPIDO", RED_DIM),
            Status::Offline => ("SEM LINK", RED_DIM),
        }
    }

    fn message_block(&self, line: &Line) -> impl IntoElement {
        let (label, label_color, body_color) = match line.role {
            Role::User => ("▌ VOCÊ", RED_DIM, BONE),
            Role::Friday => ("▌ FRIDAY", RED, RED_SOFT),
            Role::System => ("▌ SISTEMA", RED_FAINT, RED_DIM),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(label_color))
                    .child(SharedString::from(label)),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .line_height(px(21.0))
                    .text_color(rgb(body_color))
                    .child(SharedString::from(line.text.clone())),
            )
    }

    fn input_block(&self) -> impl IntoElement {
        let caret = if self.blink { "█" } else { " " };
        let prompt = format!("› {}{}", self.input, caret);
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(20.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(rgb(RED_FAINT))
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(rgb(RED))
                    .child(SharedString::from(prompt)),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(RED_FAINT))
                    .child(SharedString::from(
                        "⏎ enviar   ·   f2 falar   ·   f3 wake word ·   esc interromper",
                    )),
            )
    }
}

impl Render for FridayApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, status_color) = self.status_line();
        let clock = self.booted_at.elapsed().as_secs_f32();
        let speaking = self.voice_phase == voice::VoicePhase::Speaking;
        let busy = matches!(self.status, Status::Thinking);
        let angle = clock * if speaking { 2.4 } else { 0.22 };
        let pulse = if speaking {
            1.0 + 0.055 * (clock * 7.5).sin()
        } else {
            1.0 + 0.012 * (clock * 0.8).sin()
        };

        let orb = canvas(move |_bounds, _window, _cx| {}, move |bounds, _, window, _cx| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let cx = f32::from(bounds.origin.x) + w / 2.0;
            let cy = f32::from(bounds.origin.y) + h / 2.0;
            let base = w.min(h) * 0.44 * pulse;
            let glow = if speaking { 0x3a0d0d } else { 0x260808 };
            let dim = if speaking { 0x8f1c1c } else { 0x5e1414 };
            let bright = if speaking { 0xff4632 } else { 0xd92b23 };

            // Halo.
            paint_arc(window, cx, cy, base * 1.02, 0.0, TAU, 10.0, 0x1c0606);
            // Outer hairline ring.
            paint_arc(window, cx, cy, base, 0.0, TAU, 1.4, dim);
            // Rotating segmented outer ring.
            for i in 0..4 {
                let start = angle + i as f32 * TAU / 4.0 + 0.16;
                paint_arc(window, cx, cy, base * 0.965, start, start + 1.02, 6.0, bright);
            }
            // Dashed tick ring.
            paint_dashed_ring(window, cx, cy, base * 0.90, 2.0, 0x6d1717);
            // Counter-rotating middle ring with thicker slabs.
            for i in 0..3 {
                let start = -angle * 0.7 + i as f32 * TAU / 3.0 + 0.32;
                paint_arc(window, cx, cy, base * 0.78, start, start + 0.95, 11.0, dim);
            }
            // Inner thin arcs.
            for i in 0..3 {
                let start = angle * 1.6 + i as f32 * TAU / 3.0;
                paint_arc(window, cx, cy, base * 0.61, start, start + 0.55, 3.0, bright);
            }
            // Radial ticks around the core.
            for i in 0..48 {
                let t = i as f32 * TAU / 48.0 + angle * 0.35;
                let r1 = base * 0.50;
                let r2 = base * if i % 4 == 0 { 0.545 } else { 0.525 };
                paint_line(window, cx, cy, r1, r2, t, 1.2, if i % 4 == 0 { dim } else { glow });
            }
            // Core rings.
            paint_arc(window, cx, cy, base * 0.46, 0.0, TAU, 2.0, glow);
            paint_arc(window, cx, cy, base * 0.34, 0.0, TAU, 1.2, glow);
            paint_arc(window, cx, cy, base * 0.22, 0.0, TAU, 1.0, glow);
            // LED arc on the upper left, like the reference.
            paint_arc(
                window,
                cx,
                cy,
                base * 0.99,
                -2.55 + angle * 0.2,
                -2.05 + angle * 0.2,
                4.0,
                0x7c1a1a,
            );
        })
        .size(px(560.0));

        let answer = if !self.stream.is_empty() {
            self.stream.clone()
        } else {
            self.messages
                .iter()
                .rev()
                .find(|line| line.role == Role::Friday)
                .map(|line| line.text.clone())
                .unwrap_or_default()
        };

        let mut answer_block = div().flex().flex_col().gap(px(6.0)).w(px(270.0));
        if !answer.is_empty() {
            answer_block = answer_block.child(
                div()
                    .text_size(px(11.5))
                    .line_height(px(16.0))
                    .text_color(rgb(RED_SOFT))
                    .child(SharedString::from(answer)),
            );
        }

        let mut steps = div().flex().flex_col().gap(px(3.0));
        for activity in self.activity.iter().rev().take(4).rev() {
            let (mark, color) = match activity.state {
                StepState::Started => ("▸", RED_DIM),
                StepState::Completed => ("✓", RED_FAINT),
                StepState::Failed => ("✗", RED),
            };
            steps = steps.child(
                div()
                    .text_size(px(9.0))
                    .text_color(rgb(color))
                    .child(SharedString::from(format!("{mark} {}", activity.label))),
            );
        }

        let input_line = format!("› {}{}", self.input, if self.blink { "█" } else { " " });
        let phase_line = match self.voice_phase {
            voice::VoicePhase::Idle => "em repouso",
            voice::VoicePhase::Listening => "ouvindo",
            voice::VoicePhase::Transcribing => "transcrevendo",
            voice::VoicePhase::Speaking => "falando",
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .bg(rgb(BG))
            .text_color(rgb(RED))
            .font_family("Menlo")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key(event, cx)
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .px(px(26.0))
                    .pt(px(18.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(RED))
                            .child(SharedString::from("F R I D A Y")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(10.0))
                            .text_color(rgb(status_color))
                            .child(SharedString::from(if busy { "◉" } else { "○" }))
                            .child(SharedString::from(status)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(46.0))
                    .left(px(26.0))
                    .child(answer_block),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .relative()
                            .child(orb)
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_size(px(30.0))
                                            .text_color(rgb(RED))
                                            .child(SharedString::from("F.R.I.D.A.Y.")),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .px(px(26.0))
                    .pb(px(10.0))
                    .child(steps)
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(9.0))
                            .text_color(rgb(RED_FAINT))
                            .child(SharedString::from(
                                "⏎ enviar · f2 ouvir · f3 wake word · esc interromper",
                            ))
                            .child(SharedString::from(format!(
                                "{} ferramentas · voz: {phase_line}",
                                self.tools
                            ))),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(RED))
                            .child(SharedString::from(input_line)),
                    ),
            )
    }
}

const TAU: f32 = std::f32::consts::TAU;

fn paint_arc(
    window: &mut Window,
    cx: f32,
    cy: f32,
    radius: f32,
    start: f32,
    end: f32,
    width: f32,
    color: u32,
) {
    if radius <= 1.0 {
        return;
    }
    let steps = ((end - start).abs() * radius / 3.0).max(10.0) as usize;
    let mut builder = PathBuilder::stroke(px(width));
    for index in 0..=steps {
        let t = start + (end - start) * index as f32 / steps as f32;
        let point = point(px(cx + radius * t.cos()), px(cy + radius * t.sin()));
        if index == 0 {
            builder.move_to(point);
        } else {
            builder.line_to(point);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgb(color));
    }
}

fn paint_dashed_ring(window: &mut Window, cx: f32, cy: f32, radius: f32, width: f32, color: u32) {
    let mut builder = PathBuilder::stroke(px(width)).dash_array(&[px(2.0), px(7.0)]);
    let steps = 360usize;
    for index in 0..=steps {
        let t = index as f32 / steps as f32 * TAU;
        let point = point(px(cx + radius * t.cos()), px(cy + radius * t.sin()));
        if index == 0 {
            builder.move_to(point);
        } else {
            builder.line_to(point);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgb(color));
    }
}

fn paint_line(
    window: &mut Window,
    cx: f32,
    cy: f32,
    r1: f32,
    r2: f32,
    angle: f32,
    width: f32,
    color: u32,
) {
    let mut builder = PathBuilder::stroke(px(width));
    builder.move_to(point(px(cx + r1 * angle.cos()), px(cy + r1 * angle.sin())));
    builder.line_to(point(px(cx + r2 * angle.cos()), px(cy + r2 * angle.sin())));
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgb(color));
    }
}

/// Builds the capability manifesto from the live tool registry, so Friday knows
/// exactly what she can do — no guesswork, no "MCP not connected".
fn persona() -> String {
    let registry = friday_tools::Registry::with_defaults();
    let mut manifest = String::new();
    let mut total = 0usize;
    for spec in registry.specs() {
        total += 1;
        manifest.push_str(&format!(
            "- friday__{} [{}]: {}\n",
            spec.name,
            spec.risk.label(),
            spec.summary
        ));
    }
    format!(
        "{PERSONA_BASE}\n\n{} ferramentas conectadas agora (servidor `friday`):\n{}",
        total.max(1),
        manifest
    )
}

/// Pulls complete sentences out of a growing stream buffer, with an early
/// clause flush so the voice can start before the answer is finished.
fn take_sentences(buffer: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let mut cut = None;
        for (index, ch) in buffer.char_indices() {
            if matches!(ch, '.' | '!' | '?' | '…') {
                let after = index + ch.len_utf8();
                let rest = &buffer[after..];
                if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                    cut = Some(after);
                    break;
                }
            }
        }
        if cut.is_none() {
            let mut boundary = None;
            let mut chars = 0usize;
            for (index, ch) in buffer.char_indices() {
                if chars >= 64 {
                    break;
                }
                if matches!(ch, ',' | ';' | ':' | ' ') {
                    boundary = Some(index + ch.len_utf8());
                }
                chars += 1;
            }
            if chars >= 24 {
                if let Some(at) = boundary {
                    cut = Some(at);
                }
            }
            if cut.is_none() && buffer.chars().count() > 220 {
                let take: String = buffer.chars().take(220).collect();
                if let Some(pos) = take.rfind([',', ';', ':', ' ']) {
                    cut = Some(pos + 1);
                }
            }
        }
        match cut {
            Some(at) if at > 0 => {
                let sentence = buffer[..at].trim().to_string();
                buffer.replace_range(..at, "");
                if !sentence.is_empty() {
                    out.push(sentence);
                }
            }
            _ => break,
        }
    }
    out
}

fn shorten(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Appends to ~/Library/Logs/FridayOS.log for post-mortem debugging.
fn log(message: impl AsRef<str>) {
    use std::io::Write;
    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = std::path::PathBuf::from(home).join("Library/Logs/FridayOS.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{secs}] {}", message.as_ref());
    }
}

/// A big phosphor eye drawn with characters. It blinks, tracks, dilates with
/// whatever Friday is doing and never needs an image asset.
fn eye_art(clock: f32, energy: f32, busy: bool, speaking: bool) -> Vec<String> {
    let width = 58usize;
    let height = 21usize;
    let center_x = (width as f32 - 1.0) / 2.0;
    let center_y = (height as f32 - 1.0) / 2.0;

    // Blink every ~5.5 s, quickly.
    let blink_phase = (clock / 5.5).fract();
    let blink = if blink_phase > 0.96 {
        ((blink_phase - 0.96) / 0.04 * std::f32::consts::PI).sin()
    } else {
        0.0
    };

    let opening = (6.6 - blink * 6.0).max(0.25) * (0.85 + energy * 0.35);
    let gaze_x = (clock * 0.35).sin() * 0.22 + (clock * 1.7).sin() * 0.03 * busy as i32 as f32;
    let gaze_y = (clock * 0.5).cos() * 0.12 + if speaking { (clock * 9.0).sin() * 0.05 } else { 0.0 };
    let iris_radius = (5.2 + energy * 1.6).min(opening * 0.92);
    let pupil_radius = (2.2 + energy * 0.8).min(iris_radius * 0.62);

    let mut rows = Vec::with_capacity(height);
    for y in 0..height {
        let mut row = String::with_capacity(width);
        for x in 0..width {
            let nx = (x as f32 - center_x) / (center_x - 1.0);
            let ny = (y as f32 - center_y) / opening;
            let span = (1.0 - nx.abs().powf(1.6)).max(0.0);
            let inside = ny.abs() <= span;
            let edge = (ny.abs() - span).abs() < 0.16 && nx.abs() < 0.995;

            if !inside {
                if edge && nx.abs() < 0.99 {
                    row.push(if ny.abs() < 0.35 { '~' } else { '.' });
                } else {
                    row.push(' ');
                }
                continue;
            }

            let dx = (nx - gaze_x) * (center_x - 1.0) / 5.0;
            let dy = (ny - gaze_y) * opening / 5.0;
            let r = (dx * dx + dy * dy).sqrt();

            let ch = if r <= pupil_radius * 0.55 {
                '█'
            } else if r <= pupil_radius {
                '▓'
            } else if r <= iris_radius {
                if (r * 2.2 + clock * 3.0).sin() > 0.55 {
                    '▓'
                } else {
                    '▒'
                }
            } else if r <= iris_radius + 0.9 {
                '░'
            } else if edge {
                '#'
            } else {
                ' '
            };
            row.push(ch);
        }
        rows.push(row.trim_end().to_string());
    }
    rows
}

fn voice_label(phase: voice::VoicePhase, level: f32) -> String {
    let bars = (level * 9.0).clamp(0.0, 9.0) as usize;
    let meter: String = (0..9)
        .map(|i| if i < bars { '▮' } else { '·' })
        .collect();
    match phase {
        voice::VoicePhase::Idle => "voz · em repouso".to_string(),
        voice::VoicePhase::Listening => format!("voz · OUVINDO {meter}"),
        voice::VoicePhase::Transcribing => "voz · TRANSCREVENDO".to_string(),
        voice::VoicePhase::Speaking => format!("voz · FALANDO {meter}"),
    }
}

/// Screen capture daemon: TCC's Screen Recording grant belongs to this app, so
/// tools running in other processes ask us to take the shot.
fn start_screen_daemon() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let home = std::env::var_os("HOME").unwrap_or_default();
    let dir = std::path::PathBuf::from(home).join("Library/Application Support/FridayOS");
    let _ = std::fs::create_dir_all(&dir);
    let socket = dir.join("screen.sock");
    let _ = std::fs::remove_file(&socket);
    let Ok(listener) = UnixListener::bind(&socket) else {
        log("screen: não consegui abrir o daemon de captura");
        return;
    };
    let _ = std::fs::set_permissions(
        &socket,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    );
    log("screen: daemon de captura ativo");

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().expect("clone socket"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_str(line.trim()).unwrap_or(serde_json::Value::Null);
            let path = value
                .get("path")
                .and_then(|path| path.as_str())
                .unwrap_or_default()
                .to_string();
            let mut command = std::process::Command::new("/usr/sbin/screencapture");
            command.arg("-x");
            if let Some(region) = value.get("region").and_then(|region| region.as_str()) {
                command.args(["-R", region]);
            } else if let Some(display) = value.get("display").and_then(|display| display.as_u64()) {
                command.args(["-D", &display.to_string()]);
            }
            command.arg(&path);
            let response = match command.output() {
                Ok(output) if output.status.success() => {
                    match std::fs::metadata(&path) {
                        Ok(meta) if meta.len() > 0 => serde_json::json!({ "ok": true }),
                        _ => serde_json::json!({ "ok": false, "error": "captura vazia: verifique a permissão de Gravação da Tela" }),
                    }
                }
                Ok(output) => serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "screencapture falhou: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                }),
                Err(error) => serde_json::json!({ "ok": false, "error": error.to_string() }),
            };
            let mut stream = stream;
            let mut payload = response.to_string();
            payload.push('\n');
            let _ = stream.write_all(payload.as_bytes());
        }
    });
}

fn main() {
    if std::env::args().any(|arg| arg == "--capture-test") {
        let result = friday_tools::Registry::with_defaults().call(
            "screen_capture",
            &serde_json::json!({}),
            &friday_tools::ToolCtx::new(friday_tools::AuditLog::discover()),
        );
        let text = match result {
            Ok(value) => format!("ok {value}"),
            Err(error) => format!("erro {error}"),
        };
        let _ = std::fs::write("/tmp/friday-capture.txt", text);
        return;
    }
    if std::env::args().any(|arg| arg == "--codex-path") {
        match friday_core::codex::codex_path() {
            Some(path) => println!("{}", path.display()),
            None => println!("não encontrado"),
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--persona") {
        println!("{}", persona());
        return;
    }
    if std::env::args().any(|arg| arg == "--voices") {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = runtime.block_on(async {
            let auth = friday_core::auth::CodexAuth::discover();
            friday_core::tts::voices(&auth).await
        });
        match result {
            Ok(value) => println!("{}", serde_json::to_string(&value).unwrap_or_default()),
            Err(error) => eprintln!("{error}"),
        }
        return;
    }
    if let Some(index) = std::env::args().position(|arg| arg == "--say-voice") {
        let voice = std::env::args().nth(index + 1).unwrap_or_else(|| "viola".into());
        let text = std::env::args()
            .nth(index + 2)
            .unwrap_or_else(|| "Bom dia, senhor.".into());
        std::env::set_var("FRIDAY_VOICE", &voice);
        let cancel = Arc::new(AtomicBool::new(false));
        match voice::speak(&text, &cancel) {
            Ok(()) => println!("voz {voice}: ok"),
            Err(error) => eprintln!("voz {voice}: {error}"),
        }
        return;
    }
    if let Some(index) = std::env::args().position(|arg| arg == "--say-stream") {
        let text = std::env::args().nth(index + 1).unwrap_or_else(|| "Teste. Um, dois, três.".into());
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        for part in text.split_inclusive(['.', '!', '?']) {
            let _ = tx.send(part.trim().to_string());
        }
        drop(tx);
        let cancel = Arc::new(AtomicBool::new(false));
        match voice::speak_stream(rx, &cancel) {
            Ok(()) => println!("stream: ok"),
            Err(error) => eprintln!("stream: {error}"),
        }
        return;
    }
    if let Some(index) = std::env::args().position(|arg| arg == "--say") {
        let text = std::env::args().nth(index + 1).unwrap_or_else(|| "Friday online.".into());
        let cancel = Arc::new(AtomicBool::new(false));
        match voice::speak(&text, &cancel) {
            Ok(()) => println!("voz: ok"),
            Err(error) => eprintln!("voz: {error}"),
        }
        return;
    }
    start_screen_daemon();
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(880.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(SharedString::from("FRIDAY")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(FridayApp::new);
                window.focus(&view.read(cx).focus);
                view
            },
        )
        .expect("open FridayOS window");
        cx.activate(true);
    });
}
