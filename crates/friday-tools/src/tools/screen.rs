//! Screen perception: screenshots, Vision OCR and display inventory.
//!
//! Capture is delegated to `/usr/sbin/screencapture`; text recognition to the
//! tiny `swift/vision.swift` helper (compiled lazily by [`crate::swift`]).
//!
//! ## Coordinates
//!
//! Vision reports normalized boxes (0..1) with the origin at the **bottom-left**
//! of the image and `y` pointing up. That is useless for "click there", so
//! [`OcrBlock`] flips every box to the **top-left** origin used by macOS screen
//! and Accessibility coordinates before anything leaves this module.
//!
//! The helpers below also expose:
//! - `pixel_rect`: the same box in PNG pixels (Retina screenshots are 2x),
//! - `screen_point`: pixels converted back to logical screen points, only when
//!   the capture geometry is known (region or a full main-display shot),
//! - `scale_hint`: how many pixels per point the image uses.
//!
//! ## Permissions
//!
//! Screenshots need the Screen Recording TCC grant. Without it `screencapture`
//! may succeed yet return only the wallpaper, or fail; when it fails we say so
//! and point at System Settings. Window lookup via System Events additionally
//! needs Accessibility; if that is missing we fall back to a full-screen shot
//! and say so instead of silently pretending we captured a window.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};
use crate::swift;

const PERMISSION_HINT: &str = "Autorize o FridayOS em Ajustes do Sistema › Privacidade e Segurança › Gravação de Tela (Screen Recording) e reinicie o app; sem isso o macOS entrega uma captura vazia ou só o papel de parede.";

const COORDINATE_NOTE: &str = "O Vision devolve coordenadas normalizadas com origem no canto inferior esquerdo; a Friday já as converteu para o canto superior esquerdo (0..1). Para clicar: pixel = coordenada × pixels da imagem; ponto de tela = pixel ÷ escala.";

// ---------------------------------------------------------------------------
// Small filesystem / formatting helpers
// ---------------------------------------------------------------------------

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn shots_dir() -> PathBuf {
    home().join("Library/Caches/FridayOS/shots")
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn planned_shot_path(tag: &str) -> PathBuf {
    shots_dir().join(format!("{tag}-{}.png", now_millis()))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn expand_path(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(raw),
    }
}

/// Reads the IHDR chunk to learn the pixel size of a PNG. `None` when the file
/// is not a PNG or is too short.
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

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn parse(value: &Value) -> Result<Self, ToolError> {
        let object = value
            .as_object()
            .ok_or_else(|| ToolError::invalid("region precisa ser um objeto {x, y, w, h}"))?;

        let number = |key: &str| -> Result<f64, ToolError> {
            match object.get(key) {
                Some(value) => value
                    .as_f64()
                    .ok_or_else(|| ToolError::invalid(format!("region.{key} precisa ser numérico"))),
                None => Err(ToolError::invalid(format!("region.{key} ausente"))),
            }
        };

        let rect = Self {
            x: number("x")?,
            y: number("y")?,
            w: number("w")?,
            h: number("h")?,
        };
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return Err(ToolError::invalid(
                "region.w e region.h precisam ser maiores que zero",
            ));
        }
        Ok(rect)
    }

    /// `x,y,w,h` with integers, the shape `screencapture -R` wants.
    fn clips(&self) -> String {
        format!(
            "{},{},{},{}",
            self.x.round() as i64,
            self.y.round() as i64,
            self.w.round() as i64,
            self.h.round() as i64
        )
    }

    fn to_json(self) -> Value {
        json!({
            "x": round2(self.x),
            "y": round2(self.y),
            "width": round2(self.w),
            "height": round2(self.h),
        })
    }
}

fn optional_string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Some(_) => Err(ToolError::invalid(format!("{key} precisa ser texto"))),
    }
}

fn region_arg(args: &Value) -> Result<Option<Rect>, ToolError> {
    match args.get("region") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(Rect::parse(value)?)),
    }
}

fn display_arg(args: &Value) -> Result<Option<u32>, ToolError> {
    match args.get("display") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) if number >= 1 && number <= u32::MAX as u64 => Ok(Some(number as u32)),
            _ => Err(ToolError::invalid(
                "display precisa ser um inteiro >= 1 (1 = tela principal)",
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Screen capture
// ---------------------------------------------------------------------------

struct Shot {
    path: PathBuf,
    pixels: Option<(u32, u32)>,
    region: Option<Rect>,
    /// True when Friday created the PNG in its cache; the caller is told so it
    /// can clean up. We never delete a file the caller may still be using.
    temporary: bool,
}

fn capture_via_app(
    path: &Path,
    region: Option<Rect>,
    display: Option<u32>,
) -> Option<Result<(), ToolError>> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let home = std::env::var_os("HOME")?;
    let socket = std::path::PathBuf::from(home)
        .join("Library/Application Support/FridayOS/screen.sock");
    if !socket.exists() {
        return None;
    }
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(_) => return None,
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let request = serde_json::json!({
        "cmd": "capture",
        "path": path.display().to_string(),
        "region": region.map(|rect| rect.clips().as_str().to_string()),
        "display": display,
    });
    let mut payload = request.to_string();
    payload.push('\n');
    if stream.write_all(payload.as_bytes()).is_err() {
        return None;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("ok").and_then(|ok| ok.as_bool()).unwrap_or(false) {
        Some(Ok(()))
    } else {
        Some(Err(ToolError::failed(
            value
                .get("error")
                .and_then(|error| error.as_str())
                .unwrap_or("a Friday não conseguiu capturar a tela")
                .to_string(),
        )))
    }
}

fn capture_command(region: Option<Rect>, display: Option<u32>, path: &Path) -> Vec<String> {
    let mut parts = vec!["/usr/sbin/screencapture".to_string(), "-x".to_string()];
    if let Some(rect) = region {
        parts.push("-R".to_string());
        parts.push(rect.clips());
    } else if let Some(index) = display {
        parts.push("-D".to_string());
        parts.push(index.to_string());
    }
    parts.push(path.display().to_string());
    parts
}

fn take_shot(region: Option<Rect>, display: Option<u32>, tag: &str) -> Result<Shot, ToolError> {
    let directory = shots_dir();
    fs::create_dir_all(&directory).map_err(|error| {
        ToolError::failed(format!(
            "não consegui criar a pasta de capturas {}: {error}",
            directory.display()
        ))
    })?;

    let path = planned_shot_path(tag);

    // The app owns the Screen Recording grant. The MCP server is a separate
    // process spawned by codex, so ask the app to take the shot over a local
    // socket; fall back to direct capture for CLI/dev runs.
    if let Some(result) = capture_via_app(&path, region, display) {
        result?;
        let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if bytes == 0 {
            let _ = fs::remove_file(&path);
            return Err(ToolError::failed(format!(
                "a captura saiu vazia. {PERMISSION_HINT}"
            )));
        }
        return Ok(Shot {
            pixels: png_dims(&path),
            path,
            region,
            temporary: true,
        });
    }

    let mut command = Command::new("/usr/sbin/screencapture");
    command.arg("-x");
    if let Some(rect) = region {
        // -R uses global screen points; negative origins (secondary displays
        // left/above the main one) are accepted by screencapture.
        command.args(["-R", rect.clips().as_str()]);
    } else if let Some(index) = display {
        // -D is only meaningful for a whole display; region wins when both are
        // given, matching `capture_command`.
        command.args(["-D", index.to_string().as_str()]);
    }
    command.arg(&path);

    let output = command.output().map_err(|error| {
        ToolError::failed(format!("screencapture não pôde ser executado: {error}"))
    })?;

    if !output.status.success() {
        let _ = fs::remove_file(&path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(ToolError::failed(format!(
            "screencapture falhou ({}){}{}. {PERMISSION_HINT}",
            output.status,
            if detail.is_empty() { "" } else { ": " },
            detail
        )));
    }

    let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if bytes == 0 {
        let _ = fs::remove_file(&path);
        return Err(ToolError::failed(format!(
            "screencapture devolveu um PNG vazio. {PERMISSION_HINT}"
        )));
    }

    Ok(Shot {
        pixels: png_dims(&path),
        path,
        region,
        temporary: true,
    })
}

fn shot_json(shot: &Shot) -> Value {
    let mut value = json!({
        "path": shot.path.display().to_string(),
        "bytes": fs::metadata(&shot.path).map(|meta| meta.len()).unwrap_or(0),
        "temporary": shot.temporary,
        "coordinate_origin": "top-left",
    });
    if let Some((width, height)) = shot.pixels {
        value["width"] = json!(width);
        value["height"] = json!(height);
        value["scale_hint"] = scale_hint(shot.pixels, shot.region);
    }
    if let Some(region) = shot.region {
        value["region"] = region.to_json();
    }
    if shot.temporary {
        value["note"] = json!(
            "PNG temporário em ~/Library/Caches/FridayOS/shots; a Friday não apaga automaticamente."
        );
    }
    value
}

// ---------------------------------------------------------------------------
// Displays (system_profiler)
// ---------------------------------------------------------------------------

/// Extracts the first two numbers from strings such as `"3456 x 2234 Retina"`
/// or `"1920 x 1080 @ 60.00Hz"`.
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

fn displays_json() -> Result<Vec<Value>, ToolError> {
    let raw = crate::registry::run("/usr/sbin/system_profiler", &["SPDisplaysDataType", "-json"])?;
    let text = raw
        .as_str()
        .ok_or_else(|| ToolError::failed("system_profiler não devolveu texto"))?;
    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| ToolError::failed(format!("JSON de displays inválido: {error}")))?;

    let items = parsed
        .get("SPDisplaysDataType")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::failed("JSON de displays sem SPDisplaysDataType"))?;

    let mut displays = Vec::new();
    for item in items {
        let gpu = item
            .get("_name")
            .and_then(Value::as_str)
            .unwrap_or("GPU desconhecida");
        let Some(entries) = item.get("spdisplays_ndrvs").and_then(Value::as_array) else {
            continue;
        };

        for entry in entries {
            let name = entry
                .get("_name")
                .and_then(Value::as_str)
                .unwrap_or("Display");
            let logical = entry
                .get("spdisplays_resolution")
                .and_then(Value::as_str)
                .and_then(parse_dims);
            let pixels = entry
                .get("spdisplays_pixelresolution")
                .and_then(Value::as_str)
                .and_then(parse_dims)
                .or(logical);

            let scale = match (logical, pixels) {
                (Some((logical_width, _)), Some((pixel_width, _)))
                    if logical_width > 0.0 && pixel_width > 0.0 =>
                {
                    let ratio = pixel_width / logical_width;
                    let rounded = ratio.round();
                    if (ratio - rounded).abs() < 0.2 && rounded >= 1.0 {
                        rounded
                    } else {
                        round2(ratio)
                    }
                }
                _ => 1.0,
            };

            let flag = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .map(|value| value.contains("yes"))
                    .unwrap_or(false)
            };

            displays.push(json!({
                "gpu": gpu,
                "name": name,
                "resolution": logical.map(|(width, height)| format!("{width} x {height}")),
                "pixels": pixels.map(|(width, height)| format!("{width} x {height}")),
                "scale": scale,
                "main": flag("spdisplays_main"),
                "online": flag("spdisplays_online"),
            }));
        }
    }

    Ok(displays)
}

fn compute_main_display_logical_width() -> Option<f64> {
    let raw = crate::registry::run("/usr/sbin/system_profiler", &["SPDisplaysDataType", "-json"]).ok()?;
    let parsed: Value = serde_json::from_str(raw.as_str()?).ok()?;
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
            if width <= 0.0 {
                continue;
            }
            let main = entry
                .get("spdisplays_main")
                .and_then(Value::as_str)
                .map(|value| value.contains("yes"))
                .unwrap_or(false);
            if main {
                return Some(width);
            }
            if fallback.is_none() {
                fallback = Some(width);
            }
        }
    }
    fallback
}

fn main_display_logical_width() -> Option<f64> {
    static CACHE: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(compute_main_display_logical_width)
}

/// Best-effort pixels-per-point for an image. Region captures give an exact
/// answer; otherwise we compare the PNG width with the main display's logical
/// width (valid for full-screen shots, misleading for arbitrary crops, hence
/// the `source` field).
fn scale_hint(pixels: Option<(u32, u32)>, region: Option<Rect>) -> Value {
    if let (Some((width, _)), Some(rect)) = (pixels, region) {
        if rect.w > 0.0 {
            let ratio = width as f64 / rect.w;
            let rounded = ratio.round();
            let scale = if (ratio - rounded).abs() < 0.2 && rounded >= 1.0 {
                rounded
            } else {
                ratio
            };
            return json!({
                "scale": round2(scale),
                "source": "png_width_vs_region",
                "note": "escala exata do recorte: pixel ÷ escala = ponto de tela",
            });
        }
    }

    if let Some((width, _)) = pixels {
        if let Some(logical_width) = main_display_logical_width() {
            if logical_width > 0.0 {
                let ratio = width as f64 / logical_width;
                if (0.9..=4.0).contains(&ratio) {
                    let rounded = ratio.round();
                    let scale = if (ratio - rounded).abs() < 0.2 {
                        rounded
                    } else {
                        ratio
                    };
                    return json!({
                        "scale": round2(scale),
                        "source": "png_width_vs_main_display",
                        "note": "assume captura da tela principal inteira; recortes podem ter outra escala",
                    });
                }
            }
        }
    }

    json!({
        "scale": 1.0,
        "source": "assumed",
        "note": "escala desconhecida; em telas Retina o PNG costuma ter 2x os pontos lógicos",
    })
}

// ---------------------------------------------------------------------------
// Vision OCR
// ---------------------------------------------------------------------------

struct OcrBlock {
    text: String,
    confidence: f64,
    x: f64,
    /// Top-left origin, already flipped from Vision's bottom-left origin.
    y: f64,
    w: f64,
    h: f64,
}

impl OcrBlock {
    fn from_vision(value: &Value) -> Option<Self> {
        let text = value.get("text")?.as_str()?.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let number = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        let (x, y, w, h) = (
            number("x"),
            number("y"),
            number("width"),
            number("height"),
        );
        // Vision: origin bottom-left, y up. Flip to top-left, y down.
        let y_top = 1.0 - y - h;
        Some(Self {
            text,
            confidence: number("confidence"),
            x: x.clamp(0.0, 1.0),
            y: y_top.clamp(0.0, 1.0),
            w,
            h,
        })
    }

    fn to_json(&self, pixels: Option<(u32, u32)>, origin: Option<(f64, f64)>, scale: f64) -> Value {
        let mut value = json!({
            "text": self.text,
            "confidence": round2(self.confidence),
            "x": round4(self.x),
            "y": round4(self.y),
            "width": round4(self.w),
            "height": round4(self.h),
        });

        if let Some((image_width, image_height)) = pixels {
            let pixel_x = self.x * image_width as f64;
            let pixel_y = self.y * image_height as f64;
            let pixel_width = self.w * image_width as f64;
            let pixel_height = self.h * image_height as f64;
            value["pixel_rect"] = json!({
                "x": round2(pixel_x),
                "y": round2(pixel_y),
                "width": round2(pixel_width),
                "height": round2(pixel_height),
            });

            // Screen points only make sense when we know where the image sits
            // on the desktop. Without geometry we stay quiet rather than hand
            // out coordinates that would click the wrong place.
            if let Some((origin_x, origin_y)) = origin {
                let safe_scale = if scale > 0.0 { scale } else { 1.0 };
                value["screen_point"] = json!({
                    "x": round2(origin_x + pixel_x / safe_scale),
                    "y": round2(origin_y + pixel_y / safe_scale),
                });
            }
        }

        value
    }
}

fn ocr_file(path: &Path) -> Result<Vec<OcrBlock>, ToolError> {
    let display_path = path.to_string_lossy();
    let output = swift::run("vision", &["ocr", display_path.as_ref()])?;
    let parsed: Value = serde_json::from_str(output.trim()).map_err(|error| {
        ToolError::failed(format!("o helper Vision devolveu JSON inválido: {error}"))
    })?;
    let array = parsed.as_array().ok_or_else(|| {
        ToolError::failed("o helper Vision devolveu um JSON que não é uma lista de blocos")
    })?;
    Ok(array.iter().filter_map(OcrBlock::from_vision).collect())
}

fn ocr_payload(shot: &Shot, blocks: &[OcrBlock]) -> Value {
    let hint = scale_hint(shot.pixels, shot.region);
    let scale = hint.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
    let origin = match shot.region {
        Some(rect) => Some((rect.x, rect.y)),
        None => {
            let inferred = hint.get("source").and_then(Value::as_str)
                == Some("png_width_vs_main_display");
            if inferred {
                Some((0.0, 0.0))
            } else {
                None
            }
        }
    };

    let block_values: Vec<Value> = blocks
        .iter()
        .map(|block| block.to_json(shot.pixels, origin, scale))
        .collect();
    let text = blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut payload = json!({
        "path": shot.path.display().to_string(),
        "text": text,
        "blocks": block_values,
        "block_count": blocks.len(),
        "pixels": shot.pixels.map(|(width, height)| json!({"width": width, "height": height})),
        "temporary": shot.temporary,
        "coordinate_origin": "top-left",
        "coordinate_note": COORDINATE_NOTE,
        "scale_hint": hint,
    });

    if shot.temporary {
        payload["temp_note"] = json!(
            "PNG temporário em ~/Library/Caches/FridayOS/shots; a Friday não apaga automaticamente."
        );
    }
    payload
}

// ---------------------------------------------------------------------------
// Image resolution shared by read_text / find_text
// ---------------------------------------------------------------------------

fn resolve_image(args: &Value, ctx: &ToolCtx, tag: &str) -> Result<Shot, ToolError> {
    if let Some(raw) = optional_string(args, "path")? {
        let candidate = expand_path(raw);
        let path = if candidate.is_absolute() {
            candidate
        } else {
            ctx.cwd.join(candidate)
        };
        if !path.is_file() {
            return Err(ToolError::invalid(format!(
                "imagem não encontrada: {}",
                path.display()
            )));
        }
        return Ok(Shot {
            pixels: png_dims(&path),
            path,
            region: region_arg(args)?,
            temporary: false,
        });
    }

    let region = region_arg(args)?;
    let display = display_arg(args)?;
    take_shot(region, display, tag)
}

// ---------------------------------------------------------------------------
// Window capture (System Events geometry + screencapture -R)
// ---------------------------------------------------------------------------

fn escape_applescript(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', " ")
}

fn window_script(app: Option<&str>) -> String {
    match app {
        Some(name) => {
            let escaped = escape_applescript(name);
            format!(
                r#"tell application "System Events"
  tell process "{escaped}"
    if (count of windows) is 0 then return "NO_WINDOW"
    set p to position of window 1
    set s to size of window 1
    return "{escaped}|" & (item 1 of p) & "|" & (item 2 of p) & "|" & (item 1 of s) & "|" & (item 2 of s)
  end tell
end tell"#
            )
        }
        None => r#"tell application "System Events"
  set frontProc to first application process whose frontmost is true
  set appName to name of frontProc
  tell frontProc
    if (count of windows) is 0 then return "NO_WINDOW:" & appName
    set p to position of window 1
    set s to size of window 1
    return appName & "|" & (item 1 of p) & "|" & (item 2 of p) & "|" & (item 1 of s) & "|" & (item 2 of s)
  end tell
end tell"#
            .to_string(),
    }
}

/// Asks System Events for the frontmost window's bounds. System Events cannot
/// hand out a CGWindowID (that needs CGWindowList), so we capture the window's
/// rectangle instead. Anything overlapping the window also appears.
fn frontmost_window(app: Option<&str>) -> Result<(String, Rect), ToolError> {
    let script = window_script(app);
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", script.as_str()])
        .output()
        .map_err(|error| ToolError::failed(format!("osascript indisponível: {error}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::failed(format!(
            "System Events não respondeu: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    if line.is_empty() || line.starts_with("NO_WINDOW") {
        return Err(ToolError::failed(
            "o app alvo não tem janelas abertas (ou não está rodando)",
        ));
    }

    let mut parts = line.split('|');
    let name = parts.next().unwrap_or_default().trim().to_string();
    let numbers: Vec<f64> = parts
        .filter_map(|part| part.trim().parse::<f64>().ok())
        .collect();
    if numbers.len() < 4 {
        return Err(ToolError::failed(format!(
            "não consegui interpretar a geometria da janela: {line}"
        )));
    }

    let bounds = Rect {
        x: numbers[0],
        y: numbers[1],
        w: numbers[2],
        h: numbers[3],
    };
    if bounds.w <= 0.0 || bounds.h <= 0.0 {
        return Err(ToolError::failed("a janela tem tamanho zero"));
    }

    Ok((
        if name.is_empty() {
            app.unwrap_or("app desconhecido").to_string()
        } else {
            name
        },
        bounds,
    ))
}

// ---------------------------------------------------------------------------
// Tool specs
// ---------------------------------------------------------------------------

fn region_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "x": {"type": "number", "description": "Origem X em pontos de tela (pode ser negativa em displays à esquerda da principal)."},
            "y": {"type": "number", "description": "Origem Y em pontos de tela, a partir do topo."},
            "w": {"type": "number", "exclusiveMinimum": 0, "description": "Largura em pontos."},
            "h": {"type": "number", "exclusiveMinimum": 0, "description": "Altura em pontos."}
        },
        "required": ["x", "y", "w", "h"],
        "additionalProperties": false
    })
}

struct ScreenCapture {
    spec: ToolSpec,
}

impl ScreenCapture {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "screen_capture",
                summary: "Captura a tela (ou um recorte) para um PNG temporário e devolve o caminho e o tamanho. Exige permissão de Gravação de Tela.",
                risk: Risk::ReadOnly,
                args: json!({
                    "type": "object",
                    "properties": {
                        "region": region_schema("Recorte da tela em pontos globais."),
                        "display": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Índice do display no formato do `screencapture -D` (1 = principal). Ignorado quando region é informado."
                        }
                    },
                    "additionalProperties": false
                }),
            },
        }
    }
}

impl Tool for ScreenCapture {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let region = region_arg(args)?;
        let display = display_arg(args)?;

        if ctx.dry_run {
            let path = planned_shot_path("capture");
            return Ok(json!({
                "dry_run": true,
                "would_run": capture_command(region, display, &path),
                "path": path.display().to_string(),
                "note": "Nada foi capturado.",
            }));
        }

        let shot = take_shot(region, display, "capture")?;
        Ok(shot_json(&shot))
    }
}

struct ScreenCaptureWindow {
    spec: ToolSpec,
}

impl ScreenCaptureWindow {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "screen_capture_window",
                summary: "Captura a janela do app em primeiro plano (ou de um app específico) pela região dela. Janelas sobrepostas podem aparecer.",
                risk: Risk::ReadOnly,
                args: json!({
                    "type": "object",
                    "properties": {
                        "app": {
                            "type": "string",
                            "description": "Nome do app cuja janela capturar (ex.: \"Safari\"). Se omitido, usa o app em primeiro plano."
                        }
                    },
                    "additionalProperties": false
                }),
            },
        }
    }
}

impl Tool for ScreenCaptureWindow {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let app = optional_string(args, "app")?;

        if ctx.dry_run {
            let path = planned_shot_path("window");
            return Ok(json!({
                "dry_run": true,
                "would_query": window_script(app),
                "would_run": capture_command(None, None, &path),
                "path": path.display().to_string(),
                "note": "Nada foi consultado nem capturado.",
            }));
        }

        match frontmost_window(app) {
            Ok((name, bounds)) => {
                let shot = take_shot(Some(bounds), None, "window")?;
                let mut value = shot_json(&shot);
                value["app"] = json!(name);
                value["bounds"] = bounds.to_json();
                value["approximate"] = json!(true);
                value["note"] = json!(
                    "Captura a região do window via System Events; qualquer janela sobreposta aparece. Não existe CGWindowID por AppleScript — para isolamento exato use `screencapture -w` manualmente."
                );
                Ok(value)
            }
            Err(reason) => {
                // Better a labelled full-screen shot than a lie about windows.
                let shot = take_shot(None, None, "window-fallback")?;
                let mut value = shot_json(&shot);
                value["app"] = json!(app.unwrap_or_default());
                value["fallback"] = json!(true);
                value["reason"] = json!(reason.to_string());
                value["note"] = json!(
                    "Não consegui identificar a janela (System Events precisa de permissão de Acessibilidade); capturei a tela inteira. As coordenadas são da tela, não da janela."
                );
                Ok(value)
            }
        }
    }
}

struct ScreenReadText {
    spec: ToolSpec,
}

impl ScreenReadText {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "screen_read_text",
                summary: "Lê o texto visível na tela (ou em uma imagem) via Vision OCR e devolve o texto e os blocos com coordenadas.",
                risk: Risk::ReadOnly,
                args: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Imagem PNG/JPEG existente para ler. Se omitido, captura a tela antes."
                        },
                        "region": region_schema("Recorte a capturar quando path não é informado; se path for informado, descreve de onde a imagem veio (ajuda a calcular a escala).")
                    },
                    "additionalProperties": false
                }),
            },
        }
    }
}

impl Tool for ScreenReadText {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.dry_run {
            if let Some(path) = optional_string(args, "path")? {
                return Ok(json!({
                    "dry_run": true,
                    "would_run": ["vision", "ocr", path],
                    "note": "Nada foi capturado nem lido.",
                }));
            }
            let region = region_arg(args)?;
            let display = display_arg(args)?;
            let planned = planned_shot_path("ocr");
            return Ok(json!({
                "dry_run": true,
                "would_capture": capture_command(region, display, &planned),
                "would_run": ["vision", "ocr", planned.display().to_string()],
                "note": "Nada foi capturado nem lido.",
            }));
        }

        let shot = resolve_image(args, ctx, "ocr")?;
        let blocks = ocr_file(&shot.path)?;
        Ok(ocr_payload(&shot, &blocks))
    }
}

struct ScreenFindText {
    spec: ToolSpec,
}

impl ScreenFindText {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "screen_find_text",
                summary: "Procura um trecho de texto na tela (ou em uma imagem) e devolve os blocos que casam, com coordenadas e dica de escala.",
                risk: Risk::ReadOnly,
                args: json!({
                    "type": "object",
                    "properties": {
                        "needle": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Texto a procurar (sem diferenciar maiúsculas/minúsculas)."
                        },
                        "path": {
                            "type": "string",
                            "description": "Imagem existente onde procurar. Se omitido, captura a tela antes."
                        },
                        "region": region_schema("Recorte a capturar quando path não é informado.")
                    },
                    "required": ["needle"],
                    "additionalProperties": false
                }),
            },
        }
    }
}

impl Tool for ScreenFindText {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let needle = optional_string(args, "needle")?
            .ok_or_else(|| ToolError::invalid("needle é obrigatório e não pode ser vazio"))?;

        if ctx.dry_run {
            if let Some(path) = optional_string(args, "path")? {
                return Ok(json!({
                    "dry_run": true,
                    "needle": needle,
                    "would_run": ["vision", "ocr", path],
                    "note": "Nada foi capturado nem lido.",
                }));
            }
            let region = region_arg(args)?;
            let display = display_arg(args)?;
            let planned = planned_shot_path("find");
            return Ok(json!({
                "dry_run": true,
                "needle": needle,
                "would_capture": capture_command(region, display, &planned),
                "would_run": ["vision", "ocr", planned.display().to_string()],
                "note": "Nada foi capturado nem lido.",
            }));
        }

        let shot = resolve_image(args, ctx, "find")?;
        let blocks = ocr_file(&shot.path)?;

        let payload = ocr_payload(&shot, &blocks);
        let hint = scale_hint(shot.pixels, shot.region);
        let scale = hint.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        let origin = match shot.region {
            Some(rect) => Some((rect.x, rect.y)),
            None if hint.get("source").and_then(Value::as_str)
                == Some("png_width_vs_main_display") =>
            {
                Some((0.0, 0.0))
            }
            None => None,
        };

        let needle_lower = needle.to_lowercase();
        let matches: Vec<Value> = blocks
            .iter()
            .filter(|block| block.text.to_lowercase().contains(&needle_lower))
            .map(|block| block.to_json(shot.pixels, origin, scale))
            .collect();

        Ok(json!({
            "needle": needle,
            "match_count": matches.len(),
            "matches": matches,
            "path": shot.path.display().to_string(),
            "text": payload["text"].clone(),
            "block_count": payload["block_count"].clone(),
            "temporary": shot.temporary,
            "coordinate_origin": "top-left",
            "coordinate_note": COORDINATE_NOTE,
            "scale_hint": hint,
            "match_note": "Casamento por substring, sem diferenciar maiúsculas/minúsculas, dentro de um bloco do OCR.",
        }))
    }
}

struct ScreenDisplays {
    spec: ToolSpec,
}

impl ScreenDisplays {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "screen_displays",
                summary: "Lista os displays conectados com nome, resolução, pixels e fator de escala.",
                risk: Risk::ReadOnly,
                args: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
        }
    }
}

impl Tool for ScreenDisplays {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, _args: &Value, _ctx: &ToolCtx) -> ToolResult {
        match displays_json() {
            Ok(displays) if !displays.is_empty() => {
                let main = displays
                    .iter()
                    .find(|display| display.get("main").and_then(Value::as_bool) == Some(true))
                    .and_then(|display| display.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                Ok(json!({
                    "source": "system_profiler",
                    "count": displays.len(),
                    "main": main,
                    "displays": displays,
                }))
            }
            // Either system_profiler failed or its JSON shape changed; hand the
            // caller the readable text instead of an empty list.
            _ => match crate::registry::run("/usr/sbin/system_profiler", &["SPDisplaysDataType"]) {
                Ok(raw) => Ok(json!({
                    "source": "text_fallback",
                    "count": 0,
                    "displays": [],
                    "raw": raw.as_str().unwrap_or_default(),
                    "note": "Não consegui interpretar o JSON de displays; segue a saída bruta do system_profiler.",
                })),
                Err(error) => Err(ToolError::failed(format!(
                    "system_profiler falhou: {error}"
                ))),
            },
        }
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(ScreenCapture::new());
    registry.add(ScreenCaptureWindow::new());
    registry.add(ScreenReadText::new());
    registry.add(ScreenFindText::new());
    registry.add(ScreenDisplays::new());
}
