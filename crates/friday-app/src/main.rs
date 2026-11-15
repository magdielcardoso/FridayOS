//! FridayOS — a JARVIS-like shell.
//!
//! Black substrate, red phosphor, monospace grid. No light theme.

use std::time::Duration;

use friday_core::codex::{Codex, CodexConfig, CodexEvent};
use gpui::prelude::*;
use gpui::{
    div, px, rgb, App, Application, Bounds, Context, FocusHandle, KeyDownEvent, Render,
    ScrollHandle, SharedString, Window, WindowBounds, WindowOptions, size,
};

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

const PERSONA: &str = "\
You are Friday — the assistant of a private macOS system called FridayOS.
Speak Brazilian Portuguese by default, briefly and naturally, like a calm
voice in a dark control room. You may be dry and slightly amused, never
theatrical. Address the user as 'senhor' only when it fits; do not overdo it.
You are running as a native app and do not have tools enabled yet: never claim
to have executed commands, read files, or accessed the network. If asked for
something that would need tools, say it is not wired yet.
Never use markdown tables. Keep answers short unless asked to expand.";

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
                text: "núcleo em espera · subsistema de voz ainda não montado".into(),
            }],
            input: String::new(),
            stream: String::new(),
            reasoning: String::new(),
            status: Status::Connecting,
            thread: None,
            blink: true,
            focus,
            scroll: ScrollHandle::new(),
            booted_at: std::time::Instant::now(),
        };
        app.connect(cx);
        app
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let config = CodexConfig {
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into()),
            model: None,
            effort: Some("low".into()),
            developer_instructions: Some(PERSONA.into()),
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
                if !self.stream.trim().is_empty() {
                    let text = std::mem::take(&mut self.stream);
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
            "escape" => self.cancel_turn(cx),
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
                        "⏎ enviar   ·   esc interromper   ·   voz: em construção",
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
                "uptime {:>3}s",
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
