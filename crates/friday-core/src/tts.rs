//! Text-to-speech through ChatGPT "read aloud".
//!
//! `POST https://chatgpt.com/backend-api/read-aloud/synthesize` returns encoded
//! audio (usually AAC). We decode it to PCM16 WAV with the system `afconvert`.

use std::time::Duration;

use crate::auth::{backend_headers, AuthError, CodexAuth};
use crate::wav::{parse_wav, Pcm};

pub const READ_ALOUD_URL: &str = "https://chatgpt.com/backend-api/read-aloud/synthesize";
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("Sua sessão do ChatGPT expirou. Entre novamente pelo Codex.")]
    Unauthorized,
    #[error("A voz ChatGPT não está disponível para esta conta ou plano.")]
    Unavailable,
    #[error("O limite de uso do ChatGPT foi atingido. Tente novamente mais tarde.")]
    RateLimited,
    #[error("O serviço de voz do ChatGPT está indisponível. Tente novamente.")]
    Service,
    #[error("A voz ChatGPT não retornou áudio.")]
    Empty,
    #[error("Falha ao sintetizar voz: {0}")]
    Failed(String),
}

type Result<T> = std::result::Result<T, TtsError>;

/// Default voice. ChatGPT read-aloud accepts a `voice` name; "viola" is the
/// Brazilian-Portuguese voice used by Friday.
pub const DEFAULT_VOICE: &str = "viola";
pub const DEFAULT_LANGUAGE: &str = "pt-BR";

/// Synthesizes `text` and returns decoded PCM ready for playback.
pub async fn synthesize(auth: &CodexAuth, text: &str) -> Result<Pcm> {
    synthesize_with(auth, text, None, None).await
}

/// Same, but lets the caller pick the voice and language explicitly.
pub async fn synthesize_with(
    auth: &CodexAuth,
    text: &str,
    voice: Option<&str>,
    language: Option<&str>,
) -> Result<Pcm> {
    let clean = normalize(text);
    if clean.is_empty() {
        return Err(TtsError::Empty);
    }
    try_synthesize(auth, &clean, voice, language, false).await
}

async fn try_synthesize(
    auth: &CodexAuth,
    text: &str,
    voice: Option<&str>,
    language: Option<&str>,
    force_refresh: bool,
) -> Result<Pcm> {
    let session = if force_refresh {
        auth.force_refresh().await?
    } else {
        auth.session().await?
    };

    let voice = voice
        .map(str::to_owned)
        .or_else(|| std::env::var("FRIDAY_VOICE").ok())
        .or_else(configured_voice)
        .unwrap_or_else(|| DEFAULT_VOICE.to_string());
    let language = language
        .map(str::to_owned)
        .or_else(|| std::env::var("FRIDAY_TTS_LANGUAGE").ok())
        .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string());

    let request = backend_headers(
        reqwest::Client::new().post(READ_ALOUD_URL).json(
            &serde_json::json!({ "text": text, "voice": voice, "language": language }),
        ),
        &session,
    )
    .header("Accept", "audio/*")
    .timeout(TIMEOUT);

    let response = request
        .send()
        .await
        .map_err(|e| TtsError::Failed(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        return match code {
            401 if !force_refresh => {
                Box::pin(try_synthesize(auth, text, Some(&voice), Some(&language), true)).await
            }
            401 | 403 | 404 => Err(TtsError::Unavailable),
            429 => Err(TtsError::RateLimited),
            408 | 504 => Err(TtsError::Service),
            c if c >= 500 => Err(TtsError::Service),
            c => Err(TtsError::Failed(format!("HTTP {c}"))),
        };
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/aac")
        .split(';')
        .next()
        .unwrap_or("audio/aac")
        .trim()
        .to_lowercase();

    let bytes = response
        .bytes()
        .await
        .map_err(|e| TtsError::Failed(e.to_string()))?;
    if bytes.is_empty() {
        return Err(TtsError::Empty);
    }
    if content_type.contains("json") {
        let message = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.pointer("/detail/message")
                    .or_else(|| v.pointer("/error/message"))
                    .and_then(|m| m.as_str().map(str::to_owned))
            });
        return Err(TtsError::Failed(
            message.unwrap_or_else(|| "resposta JSON inesperada".into()),
        ));
    }

    decode_audio(&bytes, &content_type)
}

/// Voice chosen by the agent through the `voice_set` tool.
pub fn configured_voice() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::PathBuf::from(home)
        .join("Library/Application Support/FridayOS/voice.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("voice")
        .and_then(|voice| voice.as_str())
        .map(str::to_string)
}

/// Persists the voice for every future synthesis.
pub fn set_configured_voice(voice: &str, language: Option<&str>) -> std::io::Result<()> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let dir = std::path::PathBuf::from(home).join("Library/Application Support/FridayOS");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("voice.json");
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    value["voice"] = serde_json::json!(voice);
    if let Some(language) = language {
        value["language"] = serde_json::json!(language);
    }
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Uses macOS `afconvert` to turn the encoded stream into PCM16 mono WAV.
fn decode_audio(bytes: &[u8], content_type: &str) -> Result<Pcm> {
    if content_type.contains("wav") {
        return parse_wav(bytes).map_err(|e| TtsError::Failed(e.to_string()));
    }

    let dir = std::env::temp_dir().join("friday-tts");
    std::fs::create_dir_all(&dir).map_err(|e| TtsError::Failed(e.to_string()))?;
    let stem = format!("speech-{}", std::process::id());
    let input = dir.join(format!("{stem}.{}", extension(content_type)));
    let output = dir.join(format!("{stem}.wav"));

    std::fs::write(&input, bytes).map_err(|e| TtsError::Failed(e.to_string()))?;

    let status = std::process::Command::new("/usr/bin/afconvert")
        .arg("-f")
        .arg("WAVE")
        .arg("-d")
        .arg("LEI16")
        .arg("-c")
        .arg("1")
        .arg(&input)
        .arg(&output)
        .status()
        .map_err(|e| TtsError::Failed(format!("afconvert: {e}")))?;

    let result = if status.success() {
        std::fs::read(&output)
            .map_err(|e| TtsError::Failed(e.to_string()))
            .and_then(|wav| parse_wav(&wav).map_err(|e| TtsError::Failed(e.to_string())))
    } else {
        Err(TtsError::Failed("afconvert falhou".into()))
    };

    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    result
}

fn extension(content_type: &str) -> &'static str {
    if content_type.contains("mpeg") || content_type.contains("mp3") {
        "mp3"
    } else if content_type.contains("ogg") || content_type.contains("opus") {
        "ogg"
    } else if content_type.contains("mp4") || content_type.contains("m4a") {
        "m4a"
    } else if content_type.contains("flac") {
        "flac"
    } else {
        "aac"
    }
}
