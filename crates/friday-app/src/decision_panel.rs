//! Painel seguro do "Sistema de decisão" (TypeSafe Jev) quando não há chave.
//!
//! O token pode ser digitado (campo sempre mascarado) ou colado direto da
//! área de transferência via `pbpaste`, SEM exibir o conteúdo. Em ambos os
//! casos o valor vai direto para `friday_tools::decision::save_token`, o buffer
//! local é limpo e apenas mensagens genéricas aparecem no status.
//!
//! Abordagem de entrada: segue o padrão já usado em `main.rs` — um `String`
//! simples alimentado por `KeyDownEvent` (key_char/backspace/enter), em vez de
//! um `InputHandle`/elemento de input completo. Isso evita depender de APIs de
//! input do GPUI que variam entre versões e garante que o token nunca seja
//! renderizado: só `•` por caractere é desenhado.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, rgb, rgba, Context, FocusHandle, KeyDownEvent, Render, SharedString, Window,
};

/// Painel compacto, sempre no tema preto/vermelho do HUD. Sem piscadas.
pub struct DecisionPanel {
    focus: FocusHandle,
    /// Texto digitado. NUNCA renderizado — só a máscara `•` aparece.
    buffer: String,
    /// Status genérico (nunca contém o token).
    status: String,
    /// `true` enquanto `pbpaste` está em voo.
    clipboard_loading: bool,
    /// Callback opcional para o app fechar o painel após conectar.
    pub on_connected: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl DecisionPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            buffer: String::new(),
            status: "aguardando token".to_string(),
            clipboard_loading: false,
            on_connected: None,
        }
    }

    fn notify_connected(&mut self) {
        let callback = self.on_connected.clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Persiste o token e limpa o buffer local. Nunca imprime/loga o valor.
    fn connect(&mut self, cx: &mut Context<Self>) {
        let token = self.buffer.trim().to_string();
        if token.is_empty() {
            self.status = "token vazio".to_string();
            cx.notify();
            return;
        }

        let ok = friday_tools::decision::save_token(&token).is_ok();
        // Não guardamos o segredo em memória mais do que o necessário.
        self.buffer.clear();
        drop(token);

        if ok {
            self.status = "conectado".to_string();
            cx.notify();
            self.notify_connected();
        } else {
            self.status = "falha ao salvar token".to_string();
            cx.notify();
        }
    }

    /// Lê a área de transferência sem exibir nada e salva o token direto.
    fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.clipboard_loading {
            return;
        }
        self.clipboard_loading = true;
        self.status = "lendo área de transferência".to_string();
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Roda em thread de fundo; timeout curto para não travar a UI.
            let token: Option<String> = cx
                .background_executor()
                .spawn(async move {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let output = std::process::Command::new("/usr/bin/pbpaste")
                            .stdin(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .output();
                        let _ = tx.send(output);
                    });
                    match rx.recv_timeout(Duration::from_secs(2)) {
                        Ok(Ok(output)) if output.status.success() => {
                            let text = String::from_utf8(output.stdout).ok()?;
                            let trimmed = text.trim().to_string();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed)
                            }
                        }
                        _ => None,
                    }
                })
                .await;

            let _ = this.update(cx, |panel, cx| {
                panel.clipboard_loading = false;
                match token {
                    Some(value) => {
                        // Salva imediatamente; o valor nunca entra no buffer
                        // nem em status, e é descartado logo após o save.
                        let ok = friday_tools::decision::save_token(&value).is_ok();
                        drop(value);
                        panel.buffer.clear();
                        if ok {
                            panel.status = "conectado".to_string();
                            cx.notify();
                            panel.notify_connected();
                        } else {
                            panel.status = "falha ao salvar token".to_string();
                            cx.notify();
                        }
                    }
                    None => {
                        panel.status = "falha ao ler área de transferência".to_string();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Teclado: caracteres normais, espaço, backspace, Enter (conectar).
    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" => self.connect(cx),
            "backspace" => {
                self.buffer.pop();
                cx.notify();
            }
            "escape" => {
                self.buffer.clear();
                self.status = "aguardando token".to_string();
                cx.notify();
            }
            "space" => {
                self.push_text(" ");
                cx.notify();
            }
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref() {
                    self.push_text(text);
                    cx.notify();
                }
            }
        }
    }

    fn push_text(&mut self, text: &str) {
        const LIMIT: usize = 4096;
        if self.buffer.chars().count() < LIMIT {
            self.buffer.push_str(text);
        }
    }
}

impl Render for DecisionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Máscara: um `•` por caractere. O token real nunca chega ao element.
        let masked: String = self.buffer.chars().map(|_| '•').collect();
        let field = format!("› {}{}", masked, "▮");
        let status = self.status.clone();
        let loading = self.clipboard_loading;

        let bracket = |top: bool, left: bool| {
            let mut b = div()
                .absolute()
                .w(px(16.0))
                .h(px(16.0))
                .border_color(rgb(0xff2f2f));
            b = if top {
                b.top(px(0.0)).border_t_1()
            } else {
                b.bottom(px(0.0)).border_b_1()
            };
            b = if left {
                b.left(px(0.0)).border_l_1()
            } else {
                b.right(px(0.0)).border_r_1()
            };
            b
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x050505))
            .text_color(rgb(0xff2f2f))
            .font_family("Menlo")
            .track_focus(&self.focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key(event, cx)
            }))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .bg(rgba(0x070303f2))
                    .border_1()
                    .border_color(rgb(0x7a1a1a))
                    .p(px(20.0))
                    .gap(px(12.0))
                    .child(bracket(true, true))
                    .child(bracket(true, false))
                    .child(bracket(false, true))
                    .child(bracket(false, false))
                    // Cabeçalho
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .bg(rgb(0xff2f2f)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(0xff2f2f))
                                    .child(SharedString::from("SISTEMA DE DECISÃO · JEV (TYPESAFE)")),
                            ),
                    )
                    // Regra decorativa
                    .child(
                        div()
                            .text_size(px(7.0))
                            .text_color(rgb(0x4a1010))
                            .child(SharedString::from(
                                "▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏▏",
                            )),
                    )
                    // Campo mascarado
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(rgb(0xff6b6b))
                            .child(SharedString::from(field)),
                    )
                    // Status genérico (nunca o token)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(0x991f1f))
                            .child(SharedString::from(status)),
                    )
                    // Botões
                    .child(
                        div()
                            .flex()
                            .gap(px(10.0))
                            .pt(px(4.0))
                            .child(
                                div()
                                    .id("decision-connect")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(rgb(0x991f1f))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgba(0x2a0505ff)))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0xff2f2f))
                                    .child(SharedString::from("Conectar"))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _, _, cx| this.connect(cx)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("decision-clipboard")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(rgb(0x991f1f))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgba(0x2a0505ff)))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0xff6b6b))
                                    .child(SharedString::from(if loading {
                                        "Lendo..."
                                    } else {
                                        "Pegar da área de transferência"
                                    }))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _, _, cx| this.paste_from_clipboard(cx)),
                                    ),
                            ),
                    )
                    // Rodapé
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(rgb(0x3b0d0d))
                            .text_size(px(9.0))
                            .text_color(rgb(0x7a1a1a))
                            .child(SharedString::from("CANAL SEGURO · TOKEN NÃO EXIBIDO"))
                            .child(SharedString::from("⏎ conectar")),
                    ),
            )
    }
}
