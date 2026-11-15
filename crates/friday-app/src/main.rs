//! FridayOS — a JARVIS-like shell.
//!
//! Black substrate, red phosphor, monospace grid. No light theme.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use friday_core::codex::{Codex, CodexConfig, CodexEvent};
use gpui::prelude::*;
use gpui::{
    div, px, rgb, App, Application, Bounds, Context, FocusHandle, KeyDownEvent, Render,
    ScrollHandle, SharedString, Window, WindowBounds, WindowOptions, size,
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

TOOLS
A tool server named `friday` is connected and exposes the tools below. Call
them through the MCP server; the tool names are exactly as listed.
Risk policy, non-negotiable:
- read: use freely.
- reversible: act, then mention it in one sentence.
- sensitive: ask the user out loud for permission, wait for the next turn, and
  only then retry the same call with \"__confirm\": true.
- critical: same as sensitive, and say clearly what will be touched.
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
    wake: bool,
    wake_flag: Option<Arc<AtomicBool>>,
    blink: bool,
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
                text: "núcleo em espera · voz em montagem".into(),
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
            wake: false,
            wake_flag: None,
            blink: true,
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
        self.messages.push(Line {
            role: Role::System,
            text: "ouvindo…".into(),
        });
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn spawn_mic_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(120))
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
                self.voice_phase = voice::VoicePhase::Listening;
                cx.notify();
            }
            voice::MicEvent::Level(level) => {
                self.level = level;
                cx.notify();
            }
            voice::MicEvent::Utterance { path } => {
                self.level = 0.0;
                self.voice_phase = voice::VoicePhase::Transcribing;
                cx.notify();
                Self::blocking(
                    cx,
                    move || voice::transcribe_wav(&path),
                    |app, result, cx| {
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
                self.messages.push(Line {
                    role: Role::System,
                    text: format!("áudio: {error}"),
                });
                self.voice_phase = voice::VoicePhase::Idle;
                cx.notify();
            }
        }
    }

    fn speak_text(&mut self, text: String, cx: &mut Context<Self>) {
        voice::stop_speaking();
        if let Some(previous) = self.speak_cancel.take() {
            previous.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.speak_cancel = Some(cancel.clone());
        self.voice_phase = voice::VoicePhase::Speaking;
        if let Some(mic) = self.mic.as_ref() {
            mic.set_speaking(true);
        }
        cx.notify();

        Self::blocking(
            cx,
            move || voice::speak(&text, &cancel),
            |app, result, cx| {
                if let Err(error) = result {
                    app.messages.push(Line {
                        role: Role::System,
                        text: format!("voz: {error}"),
                    });
                }
                if let Some(mic) = app.mic.as_ref() {
                    mic.set_speaking(false);
                }
                app.speak_cancel = None;
                app.voice_phase = if app.mic.is_some() {
                    voice::VoicePhase::Listening
                } else {
                    voice::VoicePhase::Idle
                };
                cx.notify();
            },
        );
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
        if let Some(cancel) = self.speak_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        voice::stop_speaking();
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

        let mut extra_args = Vec::new();
        if let Some(mcp) = mcp {
            extra_args.push("-c".to_string());
            extra_args.push(format!(
                "mcp_servers.friday.command=\"{}\"",
                mcp.display()
            ));
        }

        let config = CodexConfig {
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into()),
            model: None,
            effort: Some("low".into()),
            developer_instructions: Some(persona()),
            extra_args,
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

        // Caret / presence blink.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(560))
                .await;
            let _ = this.update(cx, |app, cx| {
                app.blink = !app.blink;
                cx.notify();
            });
        })
        .detach();

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
                self.scroll.scroll_to_bottom();
            }
            CodexEvent::Reasoning(delta) => {
                self.reasoning.push_str(&delta);
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
                if status == "completed" {
                    if let Some(text) = spoken {
                        self.speak_text(text, cx);
                    }
                }
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
            "f2" => self.start_listen(cx),
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
        let thread = self
            .thread
            .as_deref()
            .map(|id| shorten(id, 18))
            .unwrap_or_else(|| "—".into());

        let mut transcript = div()
            .id("transcript")
            .flex()
            .flex_col()
            .gap(px(16.0))
            .flex_1()
            .p(px(20.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);

        for line in &self.messages {
            transcript = transcript.child(self.message_block(line));
        }

        if !self.stream.is_empty() {
            transcript = transcript.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(RED))
                            .child(SharedString::from("▌ FRIDAY")),
                    )
                    .child(
                        div()
                            .text_size(px(13.5))
                            .line_height(px(21.0))
                            .text_color(rgb(RED_SOFT))
                            .child(SharedString::from(self.stream.clone())),
                    ),
            );
        }

        let mut banner = div().flex().flex_col();
        for line in BANNER {
            banner = banner.child(
                div()
                    .text_size(px(7.5))
                    .line_height(px(9.0))
                    .text_color(rgb(RED_DIM))
                    .child(SharedString::from(line)),
            );
        }

        let header = div()
            .flex()
            .justify_between()
            .items_start()
            .px(px(20.0))
            .pt(px(18.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(banner)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(BONE))
                            .child(SharedString::from("personal assistant · build 0.1.0")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(px(11.0))
                            .text_color(rgb(status_color))
                            .child(SharedString::from(if self.status == Status::Thinking {
                                "◉"
                            } else {
                                "○"
                            }))
                            .child(SharedString::from(status)),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(RED_FAINT))
                            .child(SharedString::from(format!("thread {thread}"))),
                    ),
            );

        let divider = div().h(px(1.0)).bg(rgb(RED_FAINT)).mx(px(20.0)).mt(px(14.0));

        let footer = div()
            .flex()
            .justify_between()
            .px(px(20.0))
            .py(px(8.0))
            .border_t_1()
            .border_color(rgb(RED_FAINT))
            .text_size(px(9.5))
            .text_color(rgb(RED_DIM))
            .child(SharedString::from(format!(
                "{} ferramentas · uptime {:>3}s",
                self.tools,
                self.booted_at.elapsed().as_secs()
            )))
            .child(SharedString::from("friday·os // black terminal // no light mode"));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(RED))
            .font_family("Menlo")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key(event, cx)
            }))
            .child(header)
            .child(divider)
            .child(transcript)
            .child(self.input_block())
            .child(footer)
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
            "- {} [{}]: {}\n",
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

fn shorten(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(900.0), px(680.0)), cx);
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
