//! Multiple Codex logins, so a voice-level 429 on one account can be solved by
//! asking the user to authenticate another one in the browser.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn accounts_dir() -> PathBuf {
    data_dir().join("codex-accounts")
}

fn active_name() -> Option<String> {
    let raw = std::fs::read_to_string(accounts_dir().join("active")).ok()?;
    let name = raw.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "conta".into()
    } else {
        trimmed
    }
}

fn login_path() -> String {
    let fallback = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string();
    match Command::new("/bin/zsh")
        .args(["-lc", "printf %s \"$PATH\""])
        .output()
    {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).to_string();
            if path.trim().is_empty() {
                fallback
            } else {
                path
            }
        }
        _ => fallback,
    }
}

fn codex_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("FRIDAY_CODEX_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(output) = Command::new("/bin/zsh")
        .args(["-lc", "command -v codex"])
        .output()
    {
        if output.status.success() {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        home.join(".local/bin/codex"),
    ];
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut versions: Vec<_> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        versions.sort();
        versions.reverse();
        for version in versions {
            candidates.push(version.join("bin/codex"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Points Friday at `name`: writes the active marker and refreshes the isolated
/// CODEX_HOME symlink used by the running app.
fn activate(name: &str) -> Result<PathBuf, ToolError> {
    let slug = slug(name);
    let auth = accounts_dir().join(&slug).join("auth.json");
    if !auth.is_file() {
        return Err(ToolError::failed(format!(
            "a conta `{slug}` ainda não tem login"
        )));
    }
    let dir = accounts_dir();
    std::fs::create_dir_all(&dir).map_err(|e| ToolError::failed(e.to_string()))?;
    std::fs::write(dir.join("active"), &slug).map_err(|e| ToolError::failed(e.to_string()))?;

    let isolated = data_dir().join("codex/auth.json");
    if let Some(parent) = isolated.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::failed(e.to_string()))?;
    }
    let _ = std::fs::remove_file(&isolated);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&auth, &isolated).map_err(|e| ToolError::failed(e.to_string()))?;
    Ok(auth)
}

/// Runs `codex login` in a dedicated profile, returns the URL shown to the user
/// and waits (up to `timeout`) for the browser round-trip to finish.
fn run_login(name: &str, timeout_secs: u64) -> Result<(String, PathBuf), ToolError> {
    let binary = codex_binary().ok_or_else(|| {
        ToolError::failed("o CLI do Codex não está instalado ou não foi encontrado")
    })?;
    let slug = slug(name);
    let home = accounts_dir().join(&slug);
    std::fs::create_dir_all(&home).map_err(|e| ToolError::failed(e.to_string()))?;

    let mut child = Command::new(&binary)
        .arg("login")
        .env("CODEX_HOME", &home)
        .env("PATH", login_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::failed(format!("codex login: {e}")))?;

    let (url_tx, url_rx) = mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("https://auth.openai.com") {
                    let _ = url_tx.send(line.trim().to_string());
                }
            }
        });
    }
    let mut stderr = child.stderr.take();

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(30, 600));
    let mut url = String::new();
    loop {
        if let Ok(found) = url_rx.try_recv() {
            url = found;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let auth = home.join("auth.json");
                if auth.is_file() && !status.success() {
                    // Some builds exit 0/1 after writing the file; trust the file.
                    return Ok((url, auth));
                }
                if auth.is_file() {
                    return Ok((url, auth));
                }
                let mut error = String::new();
                if let Some(stderr) = stderr.as_mut() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut error);
                }
                return Err(ToolError::failed(format!(
                    "login não concluído ({}): {}",
                    status,
                    error.trim()
                )));
            }
            Ok(None) => {}
            Err(error) => return Err(ToolError::failed(format!("codex login: {error}"))),
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::failed(format!(
                "o login não foi concluído a tempo. Link: {url}"
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

struct CodexAccountTool {
    spec: ToolSpec,
}

impl Tool for CodexAccountTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "codex_accounts" => {
                let mut accounts = Vec::new();
                if let Ok(entries) = std::fs::read_dir(accounts_dir()) {
                    for entry in entries.filter_map(|entry| entry.ok()) {
                        let path = entry.path();
                        if !path.is_dir() {
                            continue;
                        }
                        let auth = path.join("auth.json");
                        if !auth.is_file() {
                            continue;
                        }
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let modified = auth
                            .metadata()
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|duration| duration.as_secs())
                            .unwrap_or(0);
                        accounts.push(json!({ "name": name, "auth_age_secs": modified }));
                    }
                }
                let default_exists = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".codex/auth.json")
                    .is_file();
                Ok(json!({
                    "ok": true,
                    "active": active_name(),
                    "default_account_available": default_exists,
                    "accounts": accounts,
                }))
            }
            "codex_account_current" => Ok(json!({
                "ok": true,
                "active": active_name().unwrap_or_else(|| "default".into()),
            })),
            "codex_login" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("conta2");
                let timeout = args
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300);
                if ctx.dry_run {
                    return Ok(json!({
                        "dry_run": true,
                        "would_login": slug(name),
                        "message": "abriria o navegador para autenticar outra conta Codex"
                    }));
                }
                let (url, _auth) = run_login(name, timeout)?;
                let activate_account = args
                    .get("activate")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let activated = if activate_account {
                    activate(name)?;
                    true
                } else {
                    false
                };
                Ok(json!({
                    "ok": true,
                    "account": slug(name),
                    "activated": activated,
                    "url": url,
                    "message": if activated {
                        "conta autenticada e ativada"
                    } else {
                        "conta autenticada; use codex_account_switch para ativar"
                    }
                }))
            }
            "codex_account_switch" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::invalid("informe `name`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_activate": slug(name) }));
                }
                let path = activate(name)?;
                Ok(json!({
                    "ok": true,
                    "active": slug(name),
                    "auth": path.display().to_string(),
                    "note": "a Friday recarrega o Codex e a voz em alguns segundos"
                }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> CodexAccountTool {
    CodexAccountTool {
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
        "codex_accounts",
        "Lista as contas Codex já autenticadas na Friday e qual está ativa.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "codex_account_current",
        "Diz qual conta Codex a Friday está usando agora.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "codex_login",
        "Abre o login do Codex no navegador para autenticar outra conta e, se pedido, ativa essa conta.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Apelido da conta, ex.: conta2." },
                "timeout_secs": { "type": "integer", "minimum": 30, "maximum": 600, "default": 300 },
                "activate": { "type": "boolean", "default": true }
            }
        }),
    ));
    registry.add(tool(
        "codex_account_switch",
        "Ativa uma conta Codex já autenticada (sem abrir navegador).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        }),
    ));
}
