//! Synthetic input: keyboard, mouse and scroll events.
//!
//! Everything is posted through the cached Swift helper `input`, which talks to
//! CoreGraphics. The helper needs the Accessibility permission; when it is
//! missing the error explains how to grant it.
//!
//! Coordinates are global display points with the origin at the TOP-LEFT of the
//! main display (Y grows downwards), matching what the screen tools report.

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};
use crate::swift;

/// One thin wrapper per helper subcommand.
struct InputTool {
    spec: ToolSpec,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    Trusted,
    Type,
    Key,
    Click,
    Move,
    Scroll,
    Drag,
}

impl InputTool {
    fn new(spec: ToolSpec, kind: Kind) -> Self {
        Self { spec, kind }
    }
}

impl Tool for InputTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.kind {
            // Reading the trust state has no side effects, so dry-run still asks.
            Kind::Trusted => helper(&["trusted"]),

            Kind::Type => {
                let text = required_str(args, "text")?;
                if text.is_empty() {
                    return Err(ToolError::invalid("`text` não pode ser vazio"));
                }
                if ctx.dry_run {
                    return Ok(json!({
                        "ok": true,
                        "dry_run": true,
                        "action": "type",
                        "text": text,
                        "chars": text.chars().count(),
                    }));
                }
                helper(&["type", text])
            }

            Kind::Key => {
                let combo = required_str(args, "combo")?;
                let combo = combo.trim();
                if combo.is_empty() {
                    return Err(ToolError::invalid(
                        "`combo` não pode ser vazio (ex.: \"cmd+shift+4\")",
                    ));
                }
                if ctx.dry_run {
                    return Ok(json!({
                        "ok": true,
                        "dry_run": true,
                        "action": "key",
                        "combo": combo,
                    }));
                }
                helper(&["key", combo])
            }

            Kind::Click => {
                let x = number_string(args, "x")?;
                let y = number_string(args, "y")?;
                let button = optional_str(args, "button")?.unwrap_or("left");
                let button = button.trim().to_ascii_lowercase();
                if !matches!(button.as_str(), "left" | "right" | "center" | "middle") {
                    return Err(ToolError::invalid(
                        "`button` deve ser \"left\", \"right\" ou \"center\"",
                    ));
                }
                let count = optional_i64(args, "count")?.unwrap_or(1);
                if !(1..=3).contains(&count) {
                    return Err(ToolError::invalid("`count` deve estar entre 1 e 3"));
                }
                if ctx.dry_run {
                    return Ok(json!({
                        "ok": true,
                        "dry_run": true,
                        "action": "click",
                        "x": x,
                        "y": y,
                        "button": button,
                        "count": count,
                    }));
                }
                let count = count.to_string();
                helper(&["click", x.as_str(), y.as_str(), button.as_str(), count.as_str()])
            }

            Kind::Move => {
                let x = number_string(args, "x")?;
                let y = number_string(args, "y")?;
                if ctx.dry_run {
                    return Ok(json!({
                        "ok": true,
                        "dry_run": true,
                        "action": "move",
                        "x": x,
                        "y": y,
                    }));
                }
                helper(&["move", x.as_str(), y.as_str()])
            }

            Kind::Scroll => {
                let dx = number_string(args, "dx")?;
                let dy = number_string(args, "dy")?;
                if ctx.dry_run {
                    return Ok(json!({
                        "ok": true,
                        "dry_run": true,
                        "action": "scroll",
                        "dx": dx,
                        "dy": dy,
                    }));
                }
                helper(&["scroll", dx.as_str(), dy.as_str()])
            }

            Kind::Drag => {
                let from_x = number_string(args, "from_x")?;
                let from_y = number_string(args, "from_y")?;
                let to_x = number_string(args, "to_x")?;
                let to_y = number_string(args, "to_y")?;
                if ctx.dry_run {
                    return Ok(json!({
                        "ok": true,
                        "dry_run": true,
                        "action": "drag",
                        "from": { "x": from_x, "y": from_y },
                        "to": { "x": to_x, "y": to_y },
                    }));
                }
                helper(&[
                    "drag",
                    from_x.as_str(),
                    from_y.as_str(),
                    to_x.as_str(),
                    to_y.as_str(),
                ])
            }
        }
    }
}

/// Runs the Swift helper and parses its single-line JSON reply.
fn helper(args: &[&str]) -> ToolResult {
    let stdout = swift::run("input", args)?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(ToolError::failed(
            "o helper `input` não retornou JSON; verifique a permissão de Acessibilidade",
        ));
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|e| ToolError::failed(format!("JSON inválido do helper `input`: {e}")))?;
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("falha desconhecida do helper `input`");
        return Err(ToolError::failed(message.to_string()));
    }
    Ok(value)
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid(format!("`{key}` é obrigatório e deve ser texto")))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ToolError::invalid(format!("`{key}` deve ser texto"))),
    }
}

fn optional_i64(args: &Value, key: &str) -> Result<Option<i64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("`{key}` deve ser um inteiro"))),
    }
}

/// Reads a finite number and formats it for the helper (no `.0` noise).
fn number_string(args: &Value, key: &str) -> Result<String, ToolError> {
    let value = args
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ToolError::invalid(format!("`{key}` é obrigatório e deve ser um número")))?;
    if !value.is_finite() {
        return Err(ToolError::invalid(format!("`{key}` deve ser um número finito")));
    }
    if value.abs() > 100_000.0 {
        return Err(ToolError::invalid(format!(
            "`{key}` está fora do intervalo de coordenadas razoável"
        )));
    }
    Ok(if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    })
}

pub fn register(registry: &mut Registry) {
    registry.add(InputTool::new(
        ToolSpec {
            name: "input_trusted",
            summary: "Verifica se o FridayOS tem permissão de Acessibilidade para sintetizar eventos de teclado e mouse.",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        Kind::Trusted,
    ));

    registry.add(InputTool::new(
        ToolSpec {
            name: "input_type",
            summary: "Digita um texto no app em primeiro plano, caractere por caractere, como um teclado humano.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Texto a digitar. Use \\n para Enter e \\t para Tab."
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        },
        Kind::Type,
    ));

    registry.add(InputTool::new(
        ToolSpec {
            name: "input_key",
            summary: "Pressiona uma combinação de teclas no app em primeiro plano (ex.: cmd+shift+4, enter, escape, left).",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "combo": {
                        "type": "string",
                        "description": "Combinação separada por \"+\": cmd, shift, alt, ctrl, fn + uma tecla (a-z, 0-9, enter, tab, escape, space, backspace, delete, left, right, up, down, home, end, pageup, pagedown, f1-f19). Ex.: \"cmd+shift+4\"."
                    }
                },
                "required": ["combo"],
                "additionalProperties": false
            }),
        },
        Kind::Key,
    ));

    registry.add(InputTool::new(
        ToolSpec {
            name: "input_click",
            summary: "Clica com o mouse em uma coordenada global da tela (origem no canto superior esquerdo), com botão e número de cliques opcionais.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "number",
                        "description": "Coordenada X em pontos, a partir da borda esquerda da tela principal."
                    },
                    "y": {
                        "type": "number",
                        "description": "Coordenada Y em pontos, a partir da borda superior da tela principal."
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "center"],
                        "default": "left",
                        "description": "Botão do mouse."
                    },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3,
                        "default": 1,
                        "description": "Número de cliques (2 = duplo clique)."
                    }
                },
                "required": ["x", "y"],
                "additionalProperties": false
            }),
        },
        Kind::Click,
    ));

    registry.add(InputTool::new(
        ToolSpec {
            name: "input_move",
            summary: "Move o cursor do mouse para uma coordenada global da tela sem clicar.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "number",
                        "description": "Coordenada X em pontos, origem no canto superior esquerdo da tela principal."
                    },
                    "y": {
                        "type": "number",
                        "description": "Coordenada Y em pontos, origem no canto superior esquerdo da tela principal."
                    }
                },
                "required": ["x", "y"],
                "additionalProperties": false
            }),
        },
        Kind::Move,
    ));

    registry.add(InputTool::new(
        ToolSpec {
            name: "input_scroll",
            summary: "Rola a roda do mouse no app sob o cursor; valores positivos de dy rolam o conteúdo para cima e dx para a direita.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "dx": {
                        "type": "number",
                        "description": "Deslocamento horizontal em pixels de rolagem (positivo = direita)."
                    },
                    "dy": {
                        "type": "number",
                        "description": "Deslocamento vertical em pixels de rolagem (positivo = conteúdo para cima)."
                    }
                },
                "required": ["dx", "dy"],
                "additionalProperties": false
            }),
        },
        Kind::Scroll,
    ));

    registry.add(InputTool::new(
        ToolSpec {
            name: "input_drag",
            summary: "Arrasta o mouse de um ponto a outro da tela, pressionando, movendo em passos e soltando o botão esquerdo.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "from_x": {
                        "type": "number",
                        "description": "X inicial em pontos, origem no canto superior esquerdo da tela principal."
                    },
                    "from_y": {
                        "type": "number",
                        "description": "Y inicial em pontos, origem no canto superior esquerdo da tela principal."
                    },
                    "to_x": {
                        "type": "number",
                        "description": "X final em pontos."
                    },
                    "to_y": {
                        "type": "number",
                        "description": "Y final em pontos."
                    }
                },
                "required": ["from_x", "from_y", "to_x", "to_y"],
                "additionalProperties": false
            }),
        },
        Kind::Drag,
    ));
}
