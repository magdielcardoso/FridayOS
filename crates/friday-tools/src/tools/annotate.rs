//! Screen annotations: Friday points at things with red HUD reticles.
//!
//! `screen_annotate` accepts either explicit global rects or a `needle` to
//! find on screen. The needle path takes a screenshot with `screencapture`,
//! runs the existing Vision OCR helper and converts matching text boxes into
//! screen points before handing them to the tiny AppKit helper
//! (`swift/annotate.swift`), which draws borderless, click-through reticles
//! for a few seconds.
//!
//! Coordinate notes:
//! - Input rects and the helper payload use *global screen points* with the
//!   origin at the top-left (Core Graphics style).
//! - Vision returns normalized boxes with a bottom-left origin. The helper
//!   `vision ocr` output may already carry `pixel_rect` / `screen_point` (in
//!   which case those win); otherwise we derive pixels from the PNG size and
//!   divide by the display scale to land back in points.
//!
//! Permissions:
//! - Drawing overlays needs no TCC grant. It is not Accessibility, does not
//!   capture the screen and never steals focus.
//! - The `needle` path *does* read pixels, so it needs Screen Recording.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};
use crate::swift;

const DEFAULT_SECONDS: u64 = 4;
const MAX_SECONDS: u64 = 15;
const MAX_RECTS_NEEDLE: usize = 12;
const MAX_RECTS_EXPLICIT: usize = 32;
const MAX_LABEL_CHARS: usize = 80;

const SCREEN_PERMISSION_HINT: &str = "Conceda Gravação de Tela (Screen Recording) em Ajustes do Sistema › Privacidade e Segurança para o app que executa a Friday; a anotação por `needle` depende de capturar a tela para localizar o texto.";

// ---------------------------------------------------------------------------
// Rect
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    label: Option<String>,
}

impl Rect {
    fn new(x: f64, y: f64, w: f64, h: f64, label: Option<String>) -> Option<Self> {
        if !x.is_finite() || !y.is_finite() || !w.is_finite() || !h.is_finite() {
            return None;
        }
        if w <= 0.0 || h <= 0.0 || w > 20_000.0 || h > 20_000.0 {
            return None;
        }
        Some(Self {
            x: round2(x),
            y: round2(y),
            w: round2(w),
            h: round2(h),
            label: label.map(|label| truncate(&label, MAX_LABEL_CHARS)),
        })
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "x": self.x,
            "y": self.y,
            "w": self.w,
            "h": self.h,
        });
        if let Some(label) = &self.label {
            value["label"] = json!(label);
        }
        value
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

fn parse_explicit_rects(args: &Value) -> Result<Vec<Rect>, ToolError> {
    let array = args.get("rects").and_then(Value::as_array).ok_or_else(|| {
        ToolError::invalid("informe `rects` (lista) ou `needle` (texto a procurar)")
    })?;
    if array.is_empty() {
        return Err(ToolError::invalid("`rects` está vazio"));
    }

    let mut rects = Vec::new();
    for (index, entry) in array.iter().enumerate().take(MAX_RECTS_EXPLICIT) {
        let object = entry
            .as_object()
            .ok_or_else(|| ToolError::invalid(format!("rects[{index}] precisa ser um objeto")))?;
        let number = |key: &str| -> Result<f64, ToolError> {
            object.get(key).and_then(Value::as_f64).ok_or_else(|| {
                ToolError::invalid(format!("rects[{index}].{key} precisa ser numérico"))
            })
        };
        let label = object
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string);
        let rect = Rect::new(
            number("x")?,
            number("y")?,
            number("w")?,
            number("h")?,
            label,
        )
        .ok_or_else(|| {
            ToolError::invalid(format!(
                "rects[{index}] inválido: largura/altura precisam ser > 0 e os números finitos"
            ))
        })?;
        rects.push(rect);
    }
    Ok(rects)
}

// ---------------------------------------------------------------------------
// Screenshot + OCR (needle path)
// ---------------------------------------------------------------------------

/// Removes a temporary file even when an intermediate step fails.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(prefix: &str, extension: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        Self {
            path: std::env::temp_dir().join(format!(
                "friday-{prefix}-{}-{stamp}.{extension}",
                std::process::id()
            )),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn capture_screen() -> Result<TempFile, ToolError> {
    let shot = TempFile::new("annotate-shot", "png");
    let output = Command::new("/usr/sbin/screencapture")
        .arg("-x")
        .arg(shot.path())
        .output()
        .map_err(|error| {
            ToolError::failed(format!("não consegui executar o screencapture: {error}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(ToolError::failed(format!(
            "screencapture falhou ({}){}{}. {SCREEN_PERMISSION_HINT}",
            output.status,
            if detail.is_empty() { "" } else { ": " },
            detail
        )));
    }

    let bytes = fs::metadata(shot.path())
        .map(|meta| meta.len())
        .unwrap_or(0);
    if bytes == 0 {
        return Err(ToolError::failed(format!(
            "o screencapture devolveu um PNG vazio. {SCREEN_PERMISSION_HINT}"
        )));
    }
    Ok(shot)
}

/// Reads the IHDR chunk to learn the pixel size of the screenshot.
fn png_dims(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;

    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).ok()?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
    Some((width, height))
}

fn parse_dims(text: &str) -> Option<(f64, f64)> {
    let mut numbers: Vec<f64> = Vec::new();
    let mut buffer = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() || character == '.' {
            buffer.push(character);
        } else if !buffer.is_empty() {
            if let Ok(value) = buffer.parse::<f64>() {
                numbers.push(value);
            }
            buffer.clear();
            if numbers.len() == 2 {
                break;
            }
        }
    }
    if numbers.len() < 2 && !buffer.is_empty() {
        if let Ok(value) = buffer.parse::<f64>() {
            numbers.push(value);
        }
    }
    if numbers.len() >= 2 {
        Some((numbers[0], numbers[1]))
    } else {
        None
    }
}

/// Logical width of the main display, used to turn screenshot pixels into
/// screen points. Best effort: screenshots are taken with the default
/// (main) display, which is what `screencapture -x` gives us.
fn main_display_width() -> Option<f64> {
    let output = Command::new("/usr/sbin/system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    let items = parsed.get("SPDisplaysDataType")?.as_array()?;

    let mut fallback = None;
    for item in items {
        let Some(entries) = item.get("spdisplays_ndrvs").and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let Some((width, _)) = entry
                .get("spdisplays_resolution")
                .and_then(Value::as_str)
                .and_then(parse_dims)
            else {
                continue;
            };
            let main = entry
                .get("spdisplays_main")
                .and_then(Value::as_str)
                .map(|flag| flag.contains("yes"))
                .unwrap_or(false);
            if main {
                return Some(width);
            }
            fallback.get_or_insert(width);
        }
    }
    fallback
}

fn display_scale(png_width: u32) -> f64 {
    if png_width == 0 {
        return 1.0;
    }
    let Some(logical_width) = main_display_width() else {
        return 1.0;
    };
    if logical_width <= 0.0 {
        return 1.0;
    }
    let ratio = png_width as f64 / logical_width;
    if !(0.5..=4.0).contains(&ratio) {
        return 1.0;
    }
    let rounded = ratio.round();
    let scale = if (ratio - rounded).abs() < 0.2 && rounded >= 1.0 {
        rounded
    } else {
        ratio
    };
    round2(scale)
}

fn ocr_blocks(path: &Path) -> Result<Vec<Value>, ToolError> {
    let display_path = path.to_string_lossy();
    let output = swift::run("vision", &["ocr", display_path.as_ref()])?;
    let parsed: Value = serde_json::from_str(output.trim()).map_err(|error| {
        ToolError::failed(format!("o helper Vision devolveu JSON inválido: {error}"))
    })?;
    let array = parsed.as_array().ok_or_else(|| {
        ToolError::failed("o helper Vision devolveu um JSON que não é uma lista de blocos")
    })?;
    Ok(array.clone())
}

/// Converts a Vision block into a global top-left rect in screen points.
/// Prefers `pixel_rect` / `screen_point` when the helper provides them and
/// falls back to the normalized boxes produced by `vision.swift`.
fn block_to_rect(block: &Value, image_dims: (u32, u32), scale: f64, label: &str) -> Option<Rect> {
    let object = block.as_object()?;
    let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);

    let pixel_rect = object.get("pixel_rect").filter(|value| value.is_object());
    let screen_point = object.get("screen_point").filter(|value| value.is_object());

    let (x, y, w, h) = if let Some(pixels) = pixel_rect {
        let px = number(pixels, "x")?;
        let py = number(pixels, "y")?;
        let pw = number(pixels, "width").unwrap_or(0.0);
        let ph = number(pixels, "height").unwrap_or(0.0);
        let (x, y) = if let Some(point) = screen_point {
            (number(point, "x")?, number(point, "y")?)
        } else {
            (px / scale, py / scale)
        };
        (x, y, pw / scale, ph / scale)
    } else {
        let normalized = |key: &str| object.get(key).and_then(Value::as_f64);
        let nx = normalized("x")?;
        let ny = normalized("y")?;
        let nw = normalized("width")?;
        let nh = normalized("height")?;
        if let Some(point) = screen_point {
            (
                number(point, "x")?,
                number(point, "y")?,
                nw * image_dims.0 as f64 / scale,
                nh * image_dims.1 as f64 / scale,
            )
        } else {
            (
                nx * image_dims.0 as f64 / scale,
                ny * image_dims.1 as f64 / scale,
                nw * image_dims.0 as f64 / scale,
                nh * image_dims.1 as f64 / scale,
            )
        }
    };

    Rect::new(x, y, w, h, Some(label.to_string()))
}

fn rects_for_needle(
    blocks: &[Value],
    needle: &str,
    image_dims: (u32, u32),
    scale: f64,
) -> Vec<Rect> {
    let needle = needle.to_lowercase();
    let mut rects = Vec::new();
    for block in blocks {
        if rects.len() >= MAX_RECTS_NEEDLE {
            break;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || !text.to_lowercase().contains(needle.as_str()) {
            continue;
        }
        if let Some(rect) = block_to_rect(block, image_dims, scale, text) {
            rects.push(rect);
        }
    }
    rects
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

fn annotate(rects: &[Rect], seconds: u64) -> Result<Value, ToolError> {
    let payload = json!({
        "seconds": seconds,
        "rects": rects.iter().map(Rect::to_json).collect::<Vec<_>>(),
    });

    let file = TempFile::new("annotate", "json");
    fs::write(file.path(), payload.to_string()).map_err(|error| {
        ToolError::failed(format!(
            "não consegui gravar o payload da anotação: {error}"
        ))
    })?;

    let path = file.path().to_string_lossy().to_string();
    let output = swift::run("annotate", &[path.as_str()])?;
    let value: Value = serde_json::from_str(output.trim()).map_err(|error| {
        ToolError::failed(format!(
            "o helper de anotação devolveu JSON inválido: {error}"
        ))
    })?;
    if !value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(ToolError::failed(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("o helper de anotação não desenhou os retículos")
                .to_string(),
        ));
    }
    Ok(value)
}

struct AnnotateTool {
    spec: ToolSpec,
}

impl Tool for AnnotateTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "screen_annotate" => {
                let seconds = args
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_SECONDS)
                    .clamp(1, MAX_SECONDS);

                let needle = args
                    .get("needle")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|needle| !needle.is_empty());
                let has_rects = args.get("rects").is_some();

                if needle.is_some() && has_rects {
                    return Err(ToolError::invalid(
                        "informe `rects` ou `needle`, não os dois ao mesmo tempo",
                    ));
                }

                let (rects, source, extra) = if let Some(needle) = needle {
                    let shot = capture_screen()?;
                    let dims = png_dims(shot.path()).ok_or_else(|| {
                        ToolError::failed("não consegui medir o PNG capturado pela tela")
                    })?;
                    if dims.0 == 0 || dims.1 == 0 {
                        return Err(ToolError::failed("o PNG capturado tem dimensões inválidas"));
                    }
                    let scale = display_scale(dims.0);
                    let blocks = ocr_blocks(shot.path())?;
                    let rects = rects_for_needle(&blocks, needle, dims, scale);
                    if rects.is_empty() {
                        return Ok(json!({
                            "ok": false,
                            "source": "needle",
                            "needle": needle,
                            "blocks_lidos": blocks.len(),
                            "message": format!(
                                "nenhum texto contendo \"{needle}\" foi encontrado na tela; nada foi anotado"
                            ),
                        }));
                    }
                    let extra = json!({
                        "needle": needle,
                        "matches": rects.len(),
                        "ocr_blocks": blocks.len(),
                        "scale": scale,
                        "screenshot_pixels": { "width": dims.0, "height": dims.1 },
                    });
                    (rects, "needle", extra)
                } else if has_rects {
                    (parse_explicit_rects(args)?, "rects", json!({}))
                } else {
                    return Err(ToolError::invalid(
                        "informe `rects` (lista de retângulos) ou `needle` (texto a procurar)",
                    ));
                };

                let count = rects.len();
                let planned: Vec<Value> = rects.iter().map(Rect::to_json).collect();

                if ctx.dry_run {
                    return Ok(json!({
                        "dry_run": true,
                        "ok": true,
                        "source": source,
                        "seconds": seconds,
                        "rects": planned,
                        "count": count,
                        "would_run": ["annotate", "<temp json>"],
                        "note": "nada foi desenhado; os retângulos acima são o plano",
                    }));
                }

                annotate(&rects, seconds)?;
                Ok(json!({
                    "ok": true,
                    "source": source,
                    "seconds": seconds,
                    "rects": planned,
                    "count": count,
                    "note": "retículos fecham sozinhos; desenho não rouba foco e não exige Acessibilidade",
                    "details": extra,
                }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(
    name: &'static str,
    summary: &'static str,
    risk: crate::policy::Risk,
    args: Value,
) -> AnnotateTool {
    AnnotateTool {
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
        "screen_annotate",
        "Destaca elementos na tela com retículos vermelhos estilo HUD, como o Jarvis apontando para um botão. Use `rects` com coordenadas globais em pontos (origem no canto superior esquerdo) ou `needle` para procurar um texto na tela via OCR e circular o que encontrar. Some sozinho após alguns segundos e não rouba o foco.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "additionalProperties": false,
            "oneOf": [
                { "required": ["rects"] },
                { "required": ["needle"] }
            ],
            "properties": {
                "rects": {
                    "type": "array",
                    "maxItems": MAX_RECTS_EXPLICIT,
                    "description": "Retângulos em pontos de tela globais, origem no canto superior esquerdo (estilo Core Graphics).",
                    "items": {
                        "type": "object",
                        "required": ["x", "y", "w", "h"],
                        "additionalProperties": false,
                        "properties": {
                            "x": { "type": "number", "description": "Distância em pontos da borda esquerda do desktop." },
                            "y": { "type": "number", "description": "Distância em pontos do topo do desktop." },
                            "w": { "type": "number", "exclusiveMinimum": 0 },
                            "h": { "type": "number", "exclusiveMinimum": 0 },
                            "label": { "type": "string", "description": "Rótulo curto exibido junto ao retículo." }
                        }
                    }
                },
                "needle": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Texto a localizar na tela (OCR, sem diferenciar maiúsculas). Anota até 12 ocorrências."
                },
                "seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SECONDS,
                    "default": DEFAULT_SECONDS,
                    "description": "Quanto tempo os retículos ficam visíveis."
                }
            }
        }),
    ));
}
