//! Authentication against the ChatGPT/Codex backend using the credentials the
//! Codex CLI already stores.
//!
//! Friday never asks for an API key. The subscription login lives in
//! `$CODEX_HOME/auth.json` (usually `~/.codex/auth.json`) with `auth_mode =
//! "chatgpt"`. We read it, keep the OAuth access token fresh, and write rotated
//! refresh tokens back atomically so the CLI and Friday never drift apart.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Friday's own data directory.
pub fn friday_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Application Support/FridayOS")
}

/// Where extra Codex logins live: `<data>/codex-accounts/<name>/auth.json`.
pub fn accounts_dir() -> PathBuf {
    friday_data_dir().join("codex-accounts")
}

pub fn active_account() -> Option<String> {
    let raw = std::fs::read_to_string(accounts_dir().join("active")).ok()?;
    let name = raw.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

pub fn set_active_account(name: &str) -> std::io::Result<()> {
    let dir = accounts_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("active"), name.trim())?;
    // Keep the isolated CODEX_HOME pointing at the newly selected account.
    let isolated = friday_data_dir().join("codex/auth.json");
    if let Some(parent) = isolated.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target = dir.join(name.trim()).join("auth.json");
    if isolated.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&isolated);
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &isolated)?;
    Ok(())
}

/// Resolves which auth.json Friday should use right now.
pub fn active_auth_path() -> PathBuf {
    if let Some(name) = active_account() {
        let candidate = accounts_dir().join(name).join("auth.json");
        if candidate.is_file() {
            return candidate;
        }
    }
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"));
    home.join("auth.json")
}

/// Public OAuth client id used by the Codex CLI.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Refresh a little before the token actually expires.
const REFRESH_LEEWAY_SECS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Entre com uma conta ChatGPT no Codex para a Friday usar a assinatura.")]
    Missing,
    #[error("O arquivo de autenticação do Codex está corrompido. Entre novamente pelo Codex.")]
    Corrupt,
    #[error("A conta Codex usa uma chave de API. Entre com uma conta ChatGPT para usar a assinatura.")]
    ApiKeyOnly,
    #[error("Sua sessão do ChatGPT expirou. Entre novamente pelo Codex.")]
    Expired,
    #[error("Não foi possível renovar a sessão do ChatGPT: {0}")]
    Refresh(String),
    #[error("Falha de E/S no auth do Codex: {0}")]
    Io(#[from] std::io::Error),
    #[error("Resposta inesperada do servidor de autenticação: {0}")]
    Protocol(String),
}

type Result<T> = std::result::Result<T, AuthError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default, rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<Tokens>,
    #[serde(default)]
    last_refresh: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub account_id: Option<String>,
    pub plan: Option<String>,
    pub email: Option<String>,
    pub expires_at: Option<u64>,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| exp.saturating_sub(REFRESH_LEEWAY_SECS) <= now_secs())
            .unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Handle to the local Codex credentials.
pub struct CodexAuth {
    path: PathBuf,
    cached: Mutex<Option<Session>>,
}

impl CodexAuth {
    /// Uses the active Friday account if one is selected, otherwise
    /// `$CODEX_HOME/auth.json`, falling back to `~/.codex/auth.json`.
    pub fn discover() -> Self {
        Self::at(active_auth_path())
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cached: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Returns a usable session, refreshing the OAuth token when needed.
    pub async fn session(&self) -> Result<Session> {
        let mut guard = self.cached.lock().await;
        if let Some(session) = guard.as_ref() {
            if !session.is_expired() {
                return Ok(session.clone());
            }
        }
        let (file, session) = self.read().await?;
        let session = if session.is_expired() {
            self.refresh(&file, session).await?
        } else {
            session
        };
        *guard = Some(session.clone());
        Ok(session)
    }

    /// Forces a refresh regardless of the local expiry (e.g. after a 401).
    pub async fn force_refresh(&self) -> Result<Session> {
        let mut guard = self.cached.lock().await;
        let (file, session) = self.read().await?;
        let session = self.refresh(&file, session).await?;
        *guard = Some(session.clone());
        Ok(session)
    }

    async fn read(&self) -> Result<(AuthFile, Session)> {
        let raw = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|_| AuthError::Missing)?;
        let file: AuthFile = serde_json::from_str(&raw).map_err(|_| AuthError::Corrupt)?;
        if file.tokens.is_none() && file.openai_api_key.is_some() {
            return Err(AuthError::ApiKeyOnly);
        }
        let session = session_from_file(&file)?;
        Ok((file, session))
    }

    async fn refresh(&self, file: &AuthFile, session: Session) -> Result<Session> {
        // Refresh tokens are single-use and rotate. The per-instance mutex was
        // not enough: every caller builds its own CodexAuth, so two concurrent
        // TTS/STT paths refreshed with the same token and one of them lost.
        static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        let gate = GATE.get_or_init(|| tokio::sync::Mutex::new(()));
        let _gate = gate.lock().await;
        if let Ok((_, fresh)) = self.read().await {
            if !fresh.is_expired() && fresh.refresh_token != session.refresh_token {
                // Someone else already rotated it; reuse their session.
                return Ok(fresh);
            }
        }

        let form = [
            ("grant_type", "refresh_token".to_string()),
            ("client_id", CODEX_CLIENT_ID.to_string()),
            ("refresh_token", session.refresh_token.clone()),
        ];

        let response = reqwest::Client::new()
            .post(TOKEN_URL)
            .header("Accept", "application/json")
            .form(&form)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| AuthError::Refresh(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(AuthError::Refresh(format!("HTTP {}", status.as_u16())));
        }

        let body: RefreshResponse = response
            .json()
            .await
            .map_err(|e| AuthError::Protocol(e.to_string()))?;

        let access_token = body.access_token;
        let refreshed = Session {
            expires_at: body
                .expires_in
                .map(|secs| now_secs() + secs)
                .or_else(|| jwt_exp(&access_token)),
            access_token,
            refresh_token: body.refresh_token.unwrap_or(session.refresh_token),
            id_token: body.id_token.or(session.id_token),
            account_id: session.account_id,
            plan: session.plan,
            email: session.email,
        };

        if let Some(id_token) = refreshed.id_token.as_deref() {
            let claims = Identity::from_id_token(id_token);
            // Identity claims are authoritative when present.
            let mut merged = refreshed;
            if claims.account_id.is_some() {
                merged.account_id = claims.account_id;
            }
            if claims.plan.is_some() {
                merged.plan = claims.plan;
            }
            if claims.email.is_some() {
                merged.email = claims.email;
            }
            self.persist(file, &merged).await?;
            return Ok(merged);
        }

        self.persist(file, &refreshed).await?;
        Ok(refreshed)
    }

    async fn persist(&self, previous: &AuthFile, session: &Session) -> Result<()> {
        let tokens = Tokens {
            access_token: session.access_token.clone(),
            refresh_token: session.refresh_token.clone(),
            id_token: session.id_token.clone(),
            account_id: session.account_id.clone(),
        };
        let file = AuthFile {
            auth_mode: previous
                .auth_mode
                .clone()
                .or_else(|| Some("chatgpt".to_string())),
            openai_api_key: previous.openai_api_key.clone(),
            tokens: Some(tokens),
            last_refresh: Some(iso8601(now_secs())),
        };
        let raw = serde_json::to_vec_pretty(&file).map_err(|e| AuthError::Protocol(e.to_string()))?;

        let target = resolve_symlink(&self.path);
        let parent = target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // Unique per write: two processes sharing one temp name could rename a
        // half-written file over auth.json.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = parent.join(format!(
            ".auth.json.friday-{}-{seq}",
            std::process::id()
        ));

        tokio::fs::write(&tmp, &raw).await?;
        set_private_permissions(&tmp).await?;
        tokio::fs::rename(&tmp, &target).await?;
        Ok(())
    }
}

fn resolve_symlink(path: &Path) -> PathBuf {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| std::fs::canonicalize(path).ok())
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn session_from_file(file: &AuthFile) -> Result<Session> {
    if let Some(mode) = file.auth_mode.as_deref() {
        if mode != "chatgpt" {
            return Err(AuthError::ApiKeyOnly);
        }
    }
    let tokens = file.tokens.clone().ok_or(AuthError::Missing)?;
    let identity = tokens
        .id_token
        .as_deref()
        .map(Identity::from_id_token)
        .unwrap_or_default();

    let expires_at = jwt_exp(&tokens.access_token);
    Ok(Session {
        expires_at,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        account_id: tokens.account_id.or(identity.account_id),
        plan: identity.plan,
        email: identity.email,
    })
}

#[derive(Debug, Default)]
struct Identity {
    account_id: Option<String>,
    plan: Option<String>,
    email: Option<String>,
}

impl Identity {
    fn from_id_token(token: &str) -> Self {
        let Some(payload) = jwt_payload(token) else {
            return Self::default();
        };
        let auth = payload.get("https://api.openai.com/auth");
        let profile = payload.get("https://api.openai.com/profile");
        Self {
            account_id: auth
                .and_then(|v| v.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            plan: auth
                .and_then(|v| v.get("chatgpt_plan_type"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            email: profile
                .and_then(|v| v.get("email"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        }
    }
}

fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp(token: &str) -> Option<u64> {
    jwt_payload(token)?.get("exp")?.as_u64()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn iso8601(secs: u64) -> String {
    // Minimal RFC3339 UTC formatter, avoids pulling a date crate for one field.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    // Howard Hinnant's civil_from_days.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Shared headers every ChatGPT backend call expects.
pub fn backend_headers(
    builder: reqwest::RequestBuilder,
    session: &Session,
) -> reqwest::RequestBuilder {
    let mut builder = builder
        .header("Authorization", format!("Bearer {}", session.access_token))
        .header("originator", "Codex Desktop")
        .header("User-Agent", user_agent());
    if let Some(account_id) = session.account_id.as_deref() {
        builder = builder.header("ChatGPT-Account-Id", account_id);
    }
    builder
}

pub fn user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "Mac OS",
        "windows" => "Windows NT 10.0",
        _ => "X11; Linux",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("Codex Desktop/0.1.0 ({platform}; {arch})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
