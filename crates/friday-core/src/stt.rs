//! Speech-to-text through the ChatGPT subscription backend.
//!
//! `POST https://chatgpt.com/backend-api/transcribe` with WAV PCM16 mono 16 kHz.

use std::time::Duration;

use uuid::Uuid;

use crate::auth::{backend_headers, AuthError, CodexAuth};

pub const TRANSCRIBE_URL: &str = "https://chatgpt.com/backend-api/transcribe";
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("O áudio é curto demais. Fale por alguns segundos e tente novamente.")]
    TooShort,
    #[error("Sua sessão do ChatGPT expirou. Entre novamente pelo Codex.")]
    Unauthorized,
    #[error("O limite de transcrição do ChatGPT foi atingido. Tente novamente mais tarde.")]
    RateLimited,
    #[error("O serviço de transcrição do ChatGPT está indisponível. Tente novamente.")]
    Unavailable,
    #[error("Não reconheci fala nesse áudio. Tente falar mais perto do microfone.")]
    Empty,
    #[error("Não foi possível transcrever o áudio: {0}")]
    Failed(String),
}

type Result<T> = std::result::Result<T, SttError>;

/// Uploads a WAV (PCM16 mono, 16 kHz) and returns the transcript.
pub async fn transcribe(auth: &CodexAuth, wav: Vec<u8>) -> Result<String> {
    // Parse the header instead of assuming a 44-byte PCM16/16 kHz/WAV: mobile
    // uploads used to report a meaningless duration.
    let duration_ms = crate::wav::parse_wav(&wav)
        .map(|pcm| {
            let frames = pcm.samples.len() / pcm.channels.max(1) as usize;
            frames * 1000 / pcm.sample_rate.max(1) as usize
        })
        .unwrap_or_else(|_| wav.len().saturating_sub(44) / 32);
    try_transcribe(auth, &wav, duration_ms, false).await
}

async fn try_transcribe(
    auth: &CodexAuth,
    wav: &[u8],
    duration_ms: usize,
    force_refresh: bool,
) -> Result<String> {
    let session = if force_refresh {
        auth.force_refresh().await?
    } else {
        auth.session().await?
    };

    let file_part = reqwest::multipart::Part::bytes(wav.to_vec())
        .file_name("friday-ditado.wav")
        .mime_str("audio/wav")
        .map_err(|e| SttError::Failed(e.to_string()))?;

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("language", "pt")
        .text("dictation_session_id", Uuid::new_v4().to_string())
        .text("attempt_id", Uuid::new_v4().to_string())
        .text("duration_ms", duration_ms.to_string());

    let request = backend_headers(
        reqwest::Client::new().post(TRANSCRIBE_URL).multipart(form),
        &session,
    )
    .header("Accept", "application/json")
    .timeout(TIMEOUT);

    let response = request
        .send()
        .await
        .map_err(|e| SttError::Failed(e.to_string()))?;

    let status = response.status();
    if status.is_success() {
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| SttError::Failed(e.to_string()))?;
        let text = body
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        return if text.is_empty() {
            Err(SttError::Empty)
        } else {
            Ok(text)
        };
    }

    let code = status.as_u16();
    let detail = response.text().await.unwrap_or_default();
    match code {
        401 => {
            if !force_refresh {
                return Box::pin(try_transcribe(auth, wav, duration_ms, true)).await;
            }
            Err(SttError::Unauthorized)
        }
        403 => Err(SttError::Unauthorized),
        429 => Err(SttError::RateLimited),
        408 | 504 => Err(SttError::Unavailable),
        code if code >= 500 => Err(SttError::Unavailable),
        _ if detail.contains("audio_too_short") => Err(SttError::TooShort),
        _ => Err(SttError::Failed(format!("HTTP {code}: {detail}"))),
    }
}
