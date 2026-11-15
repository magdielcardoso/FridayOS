use crate::registry::{ToolCtx, ToolError, ToolResult};

/// How much damage a tool can do, which decides whether Friday asks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Reading state: screen inventory, battery, files.
    ReadOnly = 0,
    /// Reversible: open an app, change volume, create an event.
    Reversible = 1,
    /// Sensitive: send a message, delete a file, run a shell command.
    Sensitive = 2,
    /// Critical: sudo, keychain secrets, payments, disabling security.
    Critical = 3,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::ReadOnly => "read",
            Risk::Reversible => "reversible",
            Risk::Sensitive => "sensitive",
            Risk::Critical => "critical",
        }
    }

    /// Whether a spoken "confirma?" round-trip is required.
    pub fn needs_confirmation(self) -> bool {
        self >= Risk::Sensitive
    }

    /// Whether Touch ID / passphrase is required.
    pub fn needs_biometrics(self) -> bool {
        self == Risk::Critical
    }
}


// ---------------------------------------------------------------------------
// Autonomy levels
// ---------------------------------------------------------------------------

use std::path::PathBuf;

/// How much Friday is allowed to do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autonomy {
    /// Everything that changes state needs a spoken confirmation.
    Locked,
    /// Default: sensitive and critical actions ask first.
    Standard,
    /// Full access: only destructive actions ask first.
    Full,
}

impl Autonomy {
    pub fn label(self) -> &'static str {
        match self {
            Autonomy::Locked => "locked",
            Autonomy::Standard => "standard",
            Autonomy::Full => "full",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "locked" | "travado" => Some(Autonomy::Locked),
            "standard" | "padrao" | "padrão" => Some(Autonomy::Standard),
            "full" | "total" | "acesso total" => Some(Autonomy::Full),
            _ => None,
        }
    }
}

fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/FridayOS")
        .join("autonomy.json")
}

/// Current level: env override, then persisted file, then `standard`.
pub fn current_level() -> Autonomy {
    if let Ok(value) = std::env::var("FRIDAY_AUTONOMY") {
        if let Some(level) = Autonomy::parse(&value) {
            return level;
        }
    }
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("level")
                .and_then(|level| level.as_str())
                .and_then(Autonomy::parse)
        })
        .unwrap_or(Autonomy::Standard)
}

pub fn set_level(level: Autonomy) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "level": level.label(),
            "updated": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }))?,
    )
}

/// Actions that can lose data or break the machine. These always ask, even in
/// full autonomy.
pub fn is_destructive(tool: &str) -> bool {
    matches!(
        tool,
        "fs_trash"
            | "fs_move"
            | "fs_write"
            | "process_kill"
            | "power_sleep"
            | "keychain_set"
            | "shell_run"
            | "applescript_run"
            | "jxa_run"
            | "app_quit"
            | "wifi_join"
            | "codex_login"
            | "codex_account_switch"
            | "autonomy_set"
    )
}

pub fn authorize_tool(tool: &str, risk: Risk, ctx: &ToolCtx) -> ToolResult {
    let destructive = is_destructive(tool);
    let level = current_level();
    let needs = match level {
        Autonomy::Locked => risk > Risk::ReadOnly || destructive,
        Autonomy::Standard => risk >= Risk::Sensitive || destructive,
        Autonomy::Full => destructive,
    };
    if needs && !ctx.confirmed {
        return Err(ToolError::Denied(format!(
            "`{tool}` é {}{} e precisa de confirmação explícita (autonomia: {})",
            risk.label(),
            if destructive { " e destrutiva" } else { "" },
            level.label()
        )));
    }
    Ok(serde_json::Value::Null)
}
