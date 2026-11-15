//! Floating visual panels: Friday can show reports, quotes and research on the
//! user's screen even when her own window is not focused.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn socket_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/overlay.sock")
}

fn send(payload: Value) -> ToolResult {
    let path = socket_path();
    if !path.exists() {
        return Err(ToolError::failed(
            "a janela da Friday não está aberta (socket de overlay ausente)",
        ));
    }
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| ToolError::failed(format!("overlay: {error}")))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = payload.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|error| ToolError::failed(format!("overlay: {error}")))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|error| ToolError::failed(format!("overlay: {error}")))?;
    let value: Value = serde_json::from_str(reply.trim())
        .map_err(|error| ToolError::failed(format!("overlay: resposta inválida: {error}")))?;
    if value.get("ok").and_then(|ok| ok.as_bool()).unwrap_or(false) {
        Ok(value)
    } else {
        Err(ToolError::failed(
            value
                .get("error")
                .and_then(|error| error.as_str())
                .unwrap_or("a Friday não conseguiu abrir a janela"),
        ))
    }
}

struct OverlayTool {
    spec: ToolSpec,
}

impl Tool for OverlayTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "overlay_show" => {
                let body = args
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|body| !body.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `body` com o conteúdo a mostrar"))?;
                let payload = json!({
                    "action": "show",
                    "title": args.get("title").and_then(|v| v.as_str()).unwrap_or("Friday"),
                    "body": body,
                    "anchor": args.get("anchor").and_then(|v| v.as_str()).unwrap_or("center"),
                    "width": args.get("width").and_then(|v| v.as_f64()).unwrap_or(560.0),
                    "height": args.get("height").and_then(|v| v.as_f64()).unwrap_or(380.0),
                    "offset_x": args.get("offset_x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    "offset_y": args.get("offset_y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    "seconds": args.get("seconds").and_then(|v| v.as_u64()).unwrap_or(0),
                });
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_show": payload }));
                }
                send(payload)?;
                Ok(json!({
                    "ok": true,
                    "shown": true,
                    "anchor": args.get("anchor").and_then(|v| v.as_str()).unwrap_or("center"),
                }))
            }
            "overlay_close" => {
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_close": true }));
                }
                send(json!({ "action": "close" }))?;
                Ok(json!({ "ok": true, "closed": true }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> OverlayTool {
    OverlayTool {
        spec: ToolSpec {
            name,
            summary,
            risk,
            args,
        },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "overlay_show",
        "Abre uma janela flutuante sem controles na tela do usuário para mostrar relatórios, cotações, pesquisas ou qualquer conteúdo visual. Pode escolher a posição (centro, bordas, cantos).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["body"],
            "additionalProperties": false,
            "properties": {
                "title": { "type": "string", "description": "Título curto do painel." },
                "body": { "type": "string", "description": "Texto a exibir (relatório, cotação, resumo)." },
                "anchor": {
                    "type": "string",
                    "enum": ["center", "top", "bottom", "left", "right", "top-left", "top-right", "bottom-left", "bottom-right"],
                    "default": "center"
                },
                "width": { "type": "number", "default": 560 },
                "height": { "type": "number", "default": 380 },
                "offset_x": { "type": "number", "default": 0 },
                "offset_y": { "type": "number", "default": 0 },
                "seconds": { "type": "integer", "minimum": 0, "description": "0 mantém aberto até substituir/fechar." }
            }
        }),
    ));
    registry.add(tool(
        "overlay_close",
        "Fecha qualquer janela flutuante aberta pela Friday.",
        crate::policy::Risk::Reversible,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
}
