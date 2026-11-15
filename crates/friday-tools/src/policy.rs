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

pub fn authorize(risk: Risk, ctx: &ToolCtx) -> ToolResult {
    if risk.needs_confirmation() && !ctx.confirmed {
        return Err(ToolError::Denied(format!(
            "esta ação é {} e precisa de confirmação explícita",
            risk.label()
        )));
    }
    Ok(())
}
