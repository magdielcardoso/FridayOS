//! Friday Mobile: PWA + WebSocket + Web Push, meant to be exposed through
//! Tailscale Serve. Runs inside the app process and shares the live agent.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use web_push::{
    ContentEncoding, IsahcWebPushClient, SubscriptionInfo, VapidSignatureBuilder,
    WebPushClient, WebPushMessageBuilder,
};

const COOKIE: &str = "friday_session";
const MAX_BODY: usize = 2_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatLine {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    User { text: String },
    Delta { text: String },
    Done { text: String },
    Error { text: String },
    Step { text: String },
    History { messages: Vec<ChatLine> },
}

pub struct Bridge {
    requests: Mutex<Vec<String>>,
    pub events: broadcast::Sender<Event>,
    pub history: Mutex<Vec<ChatLine>>,
    pub streaming: Mutex<String>,
}

static BRIDGE: OnceLock<Arc<Bridge>> = OnceLock::new();

fn history_path() -> PathBuf {
    data_dir().join("chat-history.json")
}

fn load_history() -> Vec<ChatLine> {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<ChatLine>>(&text).ok())
        .unwrap_or_default()
}

fn save_history(history: &[ChatLine]) {
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(text) = serde_json::to_vec_pretty(history) {
        let path = history_path();
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

pub fn bridge() -> Option<Arc<Bridge>> {
    BRIDGE.get().cloned()
}

/// Called by the UI tick: messages typed on the phone.
pub fn take_requests() -> Vec<String> {
    let Some(bridge) = bridge() else {
        return Vec::new();
    };
    let mut requests = match bridge.requests.lock() {
        Ok(requests) => requests,
        Err(_) => return Vec::new(),
    };
    std::mem::take(&mut *requests)
}

pub fn push_event(event: Event) {
    if let Some(bridge) = bridge() {
        let _ = bridge.events.send(event);
    }
}

pub fn user_message(text: &str) {
    if let Some(bridge) = bridge() {
        if let Ok(mut history) = bridge.history.lock() {
            history.push(ChatLine {
                role: "user".into(),
                text: text.into(),
            });
            let len = history.len();
            if len > 200 {
                history.drain(0..len - 200);
            }
            save_history(&history);
        }
        let _ = bridge.events.send(Event::User {
            text: text.into(),
        });
    }
}

pub fn assistant_delta(text: &str) {
    if let Some(bridge) = bridge() {
        if let Ok(mut streaming) = bridge.streaming.lock() {
            // Bounded: the buffer is only a convenience and used to grow for
            // the whole session.
            if streaming.len() < 8_192 {
                streaming.push_str(text);
            }
        }
        let _ = bridge.events.send(Event::Delta {
            text: text.into(),
        });
    }
}

pub fn assistant_done(full: &str, notify: bool) {
    if let Some(bridge) = bridge() {
        if let Ok(mut history) = bridge.history.lock() {
            history.push(ChatLine {
                role: "assistant".into(),
                text: full.into(),
            });
            let len = history.len();
            if len > 200 {
                history.drain(0..len - 200);
            }
            save_history(&history);
        }
        if let Ok(mut streaming) = bridge.streaming.lock() {
            streaming.clear();
        }
        let _ = bridge.events.send(Event::Done {
            text: full.into(),
        });
    }
    if notify {
        let snippet: String = full.chars().take(140).collect();
        notify_push("Friday", if snippet.is_empty() { "Tarefa concluída" } else { &snippet });
    }
}

pub fn assistant_error(text: &str) {
    push_event(Event::Error {
        text: text.into(),
    });
    notify_push("Friday · erro", text);
}

pub fn step(text: &str) {
    push_event(Event::Step {
        text: text.into(),
    });
}

pub fn notify_push(title: &str, body: &str) {
    let Some(state) = STATE.get() else { return };
    let state = Arc::clone(state);
    let title = title.to_string();
    let body = body.to_string();
    if let Some(handle) = RUNTIME.get() {
        handle.spawn(async move {
            let _ = send_push(&state, &title, &body, "/").await;
        });
    }
}

struct MobileState {
    pair_code: Mutex<String>,
    /// Failed pairing attempts per IP, with the first attempt time.
    pair_failures: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    /// After too many failures the code is rotated and pairing is locked.
    pair_locked_until: Mutex<Option<Instant>>,
    session_token: String,
    secure_cookie: bool,
    vapid_public: String,
    vapid_private: PathBuf,
    subscriptions: Mutex<Vec<Value>>,
    rate: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

static STATE: OnceLock<Arc<MobileState>> = OnceLock::new();
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

pub fn pair_code() -> String {
    STATE
        .get()
        .and_then(|state| state.pair_code.lock().ok().map(|code| code.clone()))
        .unwrap_or_default()
}

/// Rotates the pairing code and persists it. The phone keeps its session
/// cookie, so this only invalidates the old code.
fn rotate_pair_code(state: &MobileState) {
    let fresh: String = random_hex(3).chars().take(6).collect();
    if let Ok(mut code) = state.pair_code.lock() {
        *code = fresh.clone();
    }
    let path = data_dir().join("mobile.json");
    let mut value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    value["pair_code"] = serde_json::json!(fresh);
    if let Ok(text) = serde_json::to_vec_pretty(&value) {
        let _ = std::fs::write(&path, text);
    }
    mlog(&format!("novo código de pareamento: {fresh}"));
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn load_config() -> (String, String) {
    let path = data_dir().join("mobile.json");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            let code = value
                .get("pair_code")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let token = value
                .get("session_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !code.is_empty() && !token.is_empty() {
                return (code, token);
            }
        }
    }
    let code: String = random_hex(3)
        .chars()
        .take(6)
        .collect::<String>()
        .to_uppercase();
    let code: String = code
        .chars()
        .map(|ch| if ch.is_ascii_hexdigit() { ch } else { '0' })
        .collect();
    let token = random_hex(32);
    let _ = std::fs::create_dir_all(data_dir());
    let _ = std::fs::write(
        path,
        serde_json::to_vec_pretty(&json!({ "pair_code": code, "session_token": token }))
            .unwrap_or_default(),
    );
    let _ = std::process::Command::new("/bin/chmod")
        .args(["600", &data_dir().join("mobile.json").display().to_string()])
        .status();
    (code, token)
}

fn ensure_vapid() -> Option<(String, PathBuf)> {
    let dir = data_dir().join("push");
    let _ = std::fs::create_dir_all(&dir);
    let key = dir.join("vapid_private.pem");
    if !key.exists() {
        let status = std::process::Command::new("/usr/bin/openssl")
            .args([
                "genpkey",
                "-algorithm",
                "EC",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-out",
                &key.display().to_string(),
            ])
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let _ = std::process::Command::new("/bin/chmod")
            .args(["600", &key.display().to_string()])
            .status();
    }
    let der = std::process::Command::new("/usr/bin/openssl")
        .args([
            "ec",
            "-in",
            &key.display().to_string(),
            "-pubout",
            "-outform",
            "DER",
        ])
        .output()
        .ok()?;
    if !der.status.success() {
        return None;
    }
    let bytes = der.stdout;
    if bytes.len() < 65 {
        return None;
    }
    let point = &bytes[bytes.len() - 65..];
    let public = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(point);
    Some((public, key))
}

fn subscriptions_path() -> PathBuf {
    data_dir().join("push-subscriptions.json")
}

fn load_subscriptions() -> Vec<Value> {
    std::fs::read_to_string(subscriptions_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn save_subscriptions(items: &[Value]) {
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(text) = serde_json::to_vec_pretty(&Value::Array(items.to_vec())) {
        let _ = std::fs::write(subscriptions_path(), text);
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let mut kv = part.trim().splitn(2, '=');
        if kv.next() == Some(COOKIE) {
            return kv.next().map(str::to_string);
        }
    }
    None
}

fn authorized(state: &MobileState, headers: &HeaderMap) -> bool {
    cookie_token(headers)
        .map(|token| token == state.session_token)
        .unwrap_or(false)
}

fn origin_ok(headers: &HeaderMap) -> bool {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    match headers.get("origin").and_then(|value| value.to_str().ok()) {
        Some(origin) => origin
            .split("://")
            .nth(1)
            .map(|origin_host| origin_host == host)
            .unwrap_or(true),
        None => true,
    }
}

fn guarded(state: &MobileState, headers: &HeaderMap, addr: &SocketAddr) -> Result<(), Response> {
    if !authorized(state, headers) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }
    if headers
        .get("x-friday-request")
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err((StatusCode::FORBIDDEN, "missing request header").into_response());
    }
    if !origin_ok(headers) {
        return Err((StatusCode::FORBIDDEN, "bad origin").into_response());
    }
    let now = Instant::now();
    if let Ok(mut rate) = state.rate.lock() {
        let entry = rate.entry(addr.ip()).or_insert((now, 0));
        if now.duration_since(entry.0) > Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        if entry.1 > 120 {
            return Err((StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response());
        }
    }
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../mobile/index.html"))
}
async fn manifest() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/manifest+json")],
        include_str!("../mobile/manifest.webmanifest"),
    )
        .into_response()
}
async fn service_worker() -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/javascript"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../mobile/sw.js"),
    )
        .into_response()
}
async fn styles() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css")],
        include_str!("../mobile/styles.css"),
    )
        .into_response()
}
async fn app_js() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        include_str!("../mobile/app.js"),
    )
        .into_response()
}
async fn icon_192() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        include_bytes!("../mobile/icons/icon-192.png").to_vec(),
    )
        .into_response()
}
async fn icon_512() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        include_bytes!("../mobile/icons/icon-512.png").to_vec(),
    )
        .into_response()
}
async fn icon_badge() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        include_bytes!("../mobile/icons/badge.png").to_vec(),
    )
        .into_response()
}
async fn icon_apple() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        include_bytes!("../mobile/icons/apple-touch-icon.png").to_vec(),
    )
        .into_response()
}

async fn health(State(state): State<Arc<MobileState>>) -> Json<Value> {
    Json(json!({ "ok": true, "app": "friday-mobile", "pairing_required": true, "vapid": !state.vapid_public.is_empty() }))
}

async fn session(
    State(state): State<Arc<MobileState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    if !authorized(&state, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }
    let messages = bridge()
        .map(|bridge| bridge.history.lock().map(|h| h.clone()).unwrap_or_default())
        .unwrap_or_default();
    Ok(Json(json!({ "ok": true, "messages": messages })))
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
}

async fn pair(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PairBody>,
) -> Response {
    let addr = addr.ip();
    if let Ok(lock) = state.pair_locked_until.lock() {
        if lock.map(|until| Instant::now() < until).unwrap_or(false) {
            return (StatusCode::TOO_MANY_REQUESTS, "aguarde antes de tentar de novo")
                .into_response();
        }
    }
    let attempt = body.code.trim().to_uppercase();
    let expected = state
        .pair_code
        .lock()
        .map(|code| code.clone())
        .unwrap_or_default();
    if attempt != expected {
        let now = Instant::now();
        let mut rotate = false;
        if let Ok(mut failures) = state.pair_failures.lock() {
            let entry = failures.entry(addr).or_insert((now, 0));
            if now.duration_since(entry.0) > Duration::from_secs(600) {
                *entry = (now, 0);
            }
            entry.1 += 1;
            if entry.1 >= 5 {
                rotate = true;
                *entry = (now, 0);
            }
        }
        if rotate {
            if let Ok(mut lock) = state.pair_locked_until.lock() {
                *lock = Some(Instant::now() + Duration::from_secs(60));
            }
            rotate_pair_code(&state);
        }
        // Small delay makes brute force expensive even before the lockout.
        tokio::time::sleep(Duration::from_millis(400)).await;
        return (StatusCode::UNAUTHORIZED, "código inválido").into_response();
    }
    if let Ok(mut failures) = state.pair_failures.lock() {
        failures.remove(&addr);
    }
    // Secure cookies are dropped by browsers over plain HTTP. Tailscale Serve
    // terminates TLS and forwards X-Forwarded-Proto; use that signal, and fall
    // back to the Origin scheme.
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let origin_https = headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("https://"))
        .unwrap_or(false);
    let secure = if state.secure_cookie && (forwarded_https || origin_https) {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000{secure}",
        state.session_token
    );
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn chat(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = guarded(&state, &headers, &addr) {
        return response;
    }
    let text = body
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "texto vazio").into_response();
    }
    if let Some(bridge) = bridge() {
        if let Ok(mut requests) = bridge.requests.lock() {
            requests.push(text);
        }
        return (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response();
    }
    (StatusCode::SERVICE_UNAVAILABLE, "agente indisponível").into_response()
}

async fn transcribe(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = guarded(&state, &headers, &addr) {
        return response;
    }
    if body.len() < 1000 || body.len() > MAX_BODY {
        return (StatusCode::BAD_REQUEST, "áudio inválido").into_response();
    }
    // iOS sends m4a/ogg; the backend wants PCM16 mono 16 kHz WAV. Convert here
    // instead of uploading mislabeled bytes that transcribe as noise.
    let raw = body.to_vec();
    let bytes = match tokio::task::spawn_blocking(move || normalize_audio(raw)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            return (StatusCode::BAD_REQUEST, error).into_response();
        }
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "conversão falhou").into_response(),
    };
    // Transcrição apenas no Mac (SFSpeechRecognizer on-device).
    let text = tokio::task::spawn_blocking(move || transcribe_local(bytes)).await;
    let text = match text {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("transcrição local falhou".to_string()),
    };
    match text {
        Ok(text) => Json(json!({ "ok": true, "text": text })).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// On-device Siri/Dictation transcription of a WAV, with no network call.
fn transcribe_local(bytes: Vec<u8>) -> Result<String, String> {
    let dir = std::env::temp_dir().join("friday-mobile-upload");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!(
        "stt-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, &bytes).map_err(|error| error.to_string())?;
    let helper = friday_tools::swift::helper("speech").map_err(|error| error.to_string())?;
    let output = std::process::Command::new(helper)
        .arg("transcribe")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| error.to_string());
    let _ = std::fs::remove_file(&path);
    let output = output.map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("reconhecimento local falhou".into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().rev() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(text) = value.get("text").and_then(|value| value.as_str()) {
                if !text.trim().is_empty() {
                    return Ok(text.trim().to_string());
                }
            }
        }
    }
    Err("reconhecimento local sem texto".into())
}

/// Converts any audio the phone sends into PCM16 mono 16 kHz WAV.
fn normalize_audio(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.len() > 44 && &bytes[0..4] == b"RIFF" {
        // Already a WAV: resample only if needed.
        let pcm = friday_core::wav::parse_wav(&bytes).map_err(|e| e.to_string())?;
        if pcm.sample_rate == 16_000 && pcm.channels == 1 {
            return Ok(bytes);
        }
    }
    let extension = if bytes.starts_with(b"OggS") {
        "ogg"
    } else if bytes.starts_with(b"fLaC") {
        "flac"
    } else {
        "m4a"
    };
    let dir = std::env::temp_dir().join("friday-mobile-upload");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem = format!(
        "upload-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let input = dir.join(format!("{stem}.{extension}"));
    let output = dir.join(format!("{stem}.wav"));
    std::fs::write(&input, &bytes).map_err(|e| e.to_string())?;
    let status = std::process::Command::new("/usr/bin/afconvert")
        .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
        .arg(&input)
        .arg(&output)
        .status()
        .map_err(|e| format!("afconvert: {e}"))?;
    let result = if status.success() {
        std::fs::read(&output).map_err(|e| e.to_string())
    } else {
        Err("formato de áudio não suportado".to_string())
    };
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    result
}

async fn ws_handler(
    State(state): State<Arc<MobileState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorized(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if !headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .map(|origin| {
            origin
                .split("://")
                .nth(1)
                .map(|host| {
                    headers
                        .get("host")
                        .and_then(|value| value.to_str().ok())
                        .map(|our| our == host)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
    {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    ws.on_upgrade(move |socket| stream_ws(socket))
}

/// Sends the whole history; also used to resynchronize after the broadcast
/// channel lagged.
async fn send_history(
    socket: &mut axum::extract::ws::WebSocket,
    bridge: &Bridge,
) -> Result<(), axum::Error> {
    let history = bridge.history.lock().map(|h| h.clone()).unwrap_or_default();
    let message = serde_json::to_string(&Event::History { messages: history })
        .unwrap_or_else(|_| "{}".into());
    socket.send(WsMessage::Text(message.into())).await
}

async fn stream_ws(mut socket: axum::extract::ws::WebSocket) {
    let Some(bridge) = bridge() else {
        return;
    };
    // Subscribe before the snapshot: events emitted while the history was
    // being serialized used to be lost.
    let mut receiver = bridge.events.subscribe();
    if send_history(&mut socket, &bridge).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                // Notice client close/ping instead of keeping a dead
                // subscription alive forever.
                match incoming {
                    Some(Ok(_)) => continue,
                    _ => break,
                }
            }
            event = receiver.recv() => match event {
                Ok(event) => {
                    let Ok(text) = serde_json::to_string(&event) else {
                        continue;
                    };
                    if socket.send(WsMessage::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Resynchronize the phone instead of leaving it blank.
                    if send_history(&mut socket, &bridge).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
}

async fn public_key(State(state): State<Arc<MobileState>>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Json(json!({ "key": state.vapid_public })).into_response()
}

async fn subscribe(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(subscription): Json<Value>,
) -> Response {
    if let Err(response) = guarded(&state, &headers, &addr) {
        return response;
    }
    let endpoint = subscription.get("endpoint").and_then(|v| v.as_str());
    let p256dh = subscription.pointer("/keys/p256dh").and_then(|v| v.as_str());
    let auth = subscription.pointer("/keys/auth").and_then(|v| v.as_str());
    if endpoint.is_none() || p256dh.is_none() || auth.is_none() {
        return (StatusCode::BAD_REQUEST, "subscription inválida").into_response();
    }
    let mut items = load_subscriptions();
    items.retain(|item| item.get("endpoint").and_then(|v| v.as_str()) != endpoint);
    items.push(subscription);
    save_subscriptions(&items);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

async fn unsubscribe(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = guarded(&state, &headers, &addr) {
        return response;
    }
    let endpoint = body.get("endpoint").and_then(|v| v.as_str());
    let mut items = load_subscriptions();
    let before = items.len();
    if let Some(endpoint) = endpoint {
        items.retain(|item| item.get("endpoint").and_then(|v| v.as_str()) != Some(endpoint));
    } else {
        items.clear();
    }
    save_subscriptions(&items);
    Json(json!({ "ok": true, "removed": before - items.len() })).into_response()
}

async fn push_test(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guarded(&state, &headers, &addr) {
        return response;
    }
    let result = send_push(&state, "Friday", "Notificação de teste. Se você está lendo isso, o push funciona.", "/").await;
    Json(result).into_response()
}

async fn send_push(state: &MobileState, title: &str, body: &str, url: &str) -> Value {
    let subscriptions = load_subscriptions();
    if subscriptions.is_empty() {
        return json!({ "ok": false, "error": "nenhuma inscrição" });
    }
    let payload = serde_json::to_vec(&json!({ "title": title, "body": body, "url": url }))
        .unwrap_or_default();
    let client = match IsahcWebPushClient::new() {
        Ok(client) => client,
        Err(error) => return json!({ "ok": false, "error": error.to_string() }),
    };
    let mut sent = 0;
    let mut failed = 0;
    let mut alive: Vec<Value> = Vec::new();
    for subscription in subscriptions {
        let endpoint = subscription
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let p256dh = subscription
            .pointer("/keys/p256dh")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let auth = subscription
            .pointer("/keys/auth")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if endpoint.is_empty() || p256dh.is_empty() || auth.is_empty() {
            continue;
        }
        let info = SubscriptionInfo::new(endpoint.clone(), p256dh.to_string(), auth.to_string());
        let file = match std::fs::File::open(&state.vapid_private) {
            Ok(file) => file,
            Err(_) => return json!({ "ok": false, "error": "chave VAPID ausente" }),
        };
        let signature = match VapidSignatureBuilder::from_pem(file, &info) {
            Ok(builder) => match builder.build() {
                Ok(signature) => signature,
                Err(error) => {
                    failed += 1;
                    let _ = error;
                    continue;
                }
            },
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm, &payload);
        builder.set_vapid_signature(signature);
        let message = match builder.build() {
            Ok(message) => message,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        match client.send(message).await {
            Ok(()) => {
                sent += 1;
                alive.push(subscription);
            }
            Err(error) => {
                failed += 1;
                let text = error.to_string();
                if !text.contains("410") && !text.contains("404") {
                    alive.push(subscription);
                }
            }
        }
    }
    // Merge instead of overwriting: a subscription registered while the pushes
    // were in flight used to be deleted by this stale snapshot.
    let known: std::collections::HashSet<String> = alive
        .iter()
        .filter_map(|item| item.get("endpoint").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let mut merged = alive;
    for item in load_subscriptions() {
        let endpoint = item
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !endpoint.is_empty() && !known.contains(&endpoint) {
            merged.push(item);
        }
    }
    save_subscriptions(&merged);
    json!({ "ok": sent > 0, "sent": sent, "failed": failed })
}

fn router(state: Arc<MobileState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/styles.css", get(styles))
        .route("/app.js", get(app_js))
        .route("/icons/icon-192.png", get(icon_192))
        .route("/icons/icon-512.png", get(icon_512))
        .route("/icons/badge.png", get(icon_badge))
        .route("/icons/apple-touch-icon.png", get(icon_apple))
        .route("/api/health", get(health))
        .route("/api/session", get(session))
        .route("/api/pair", post(pair))
        .route("/api/chat", post(chat))
        .route("/api/voice/transcribe", post(transcribe))
        .route("/api/chat/ws", get(ws_handler))
        .route("/api/push/public-key", get(public_key))
        .route("/api/push/subscribe", post(subscribe))
        .route("/api/push/unsubscribe", post(unsubscribe))
        .route("/api/push/test", post(test_guard))
        .with_state(state)
}

async fn test_guard(
    State(state): State<Arc<MobileState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    push_test(State(state), ConnectInfo(addr), headers).await
}

/// Starts the server. Returns the pairing code so the UI can show it.
fn mlog(message: &str) {
    use std::io::Write;
    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = std::path::PathBuf::from(home).join("Library/Logs/FridayOS.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{secs}] mobile: {message}");
    }
}

pub fn start(port: u16) -> Option<String> {
    let (pair_code, session_token) = load_config();
    let Some((vapid_public, vapid_private)) = ensure_vapid() else {
        mlog("VAPID indisponível; servidor móvel não iniciado");
        return None;
    };
    mlog("VAPID pronto");
    let state = Arc::new(MobileState {
        pair_code: Mutex::new(pair_code.clone()),
        pair_failures: Mutex::new(HashMap::new()),
        pair_locked_until: Mutex::new(None),
        session_token,
        secure_cookie: std::env::var("FRIDAY_MOBILE_INSECURE").is_err(),
        vapid_public,
        vapid_private,
        subscriptions: Mutex::new(Vec::new()),
        rate: Mutex::new(HashMap::new()),
    });
    let _ = STATE.set(Arc::clone(&state));

    let bridge = Arc::new(Bridge {
        requests: Mutex::new(Vec::new()),
        events: broadcast::channel(256).0,
        history: Mutex::new(load_history()),
        streaming: Mutex::new(String::new()),
    });
    let _ = BRIDGE.set(Arc::clone(&bridge));

    std::thread::Builder::new()
        .name("friday-mobile".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            let _ = RUNTIME.set(runtime.handle().clone());
            runtime.block_on(async move {
                // Localhost by default. FRIDAY_MOBILE_BIND may point at the
                // Tailscale IP for direct (non-Serve) access during setup.
                let ip: IpAddr = std::env::var("FRIDAY_MOBILE_BIND")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(IpAddr::from([127, 0, 0, 1]));
                let address = SocketAddr::new(ip, port);
                let mut listener = match tokio::net::TcpListener::bind(address).await {
                    Ok(listener) => {
                        mlog(&format!("escutando em {address}"));
                        listener
                    }
                    Err(error) => {
                        // Tailscale may be stopped; never leave mobile offline
                        // on localhost because of that.
                        mlog(&format!("bind {address} falhou ({error}); tentando 127.0.0.1"));
                        match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await {
                            Ok(listener) => listener,
                            Err(error) => {
                                // Say it out loud: the UI used to show a pairing
                                // code for a server that never bound.
                                mlog(&format!("servidor móvel indisponível: {error}"));
                                return;
                            }
                        }
                    }
                };
                let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
                let _ = axum::serve(listener, app).await;
            });
        })
        .ok();

    Some(pair_code)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);
#[allow(dead_code)]
fn next_id() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
