use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::audit::AuditLog;
use crate::policy::{self, Risk};

pub type ToolResult = Result<Value, ToolError>;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("ação negada: {0}")]
    Denied(String),
    #[error("argumentos inválidos: {0}")]
    InvalidArgs(String),
    #[error("ferramenta falhou: {0}")]
    Failed(String),
    #[error("ferramenta não encontrada: {0}")]
    NotFound(String),
}

impl ToolError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgs(message.into())
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub risk: Risk,
    /// JSON Schema for the arguments, shown to the model / MCP client.
    pub args: Value,
}

pub struct ToolCtx {
    /// The voice/UI layer already confirmed this specific action.
    pub confirmed: bool,
    /// Do not perform side effects; report what would happen.
    pub dry_run: bool,
    pub audit: AuditLog,
    pub cwd: PathBuf,
}

impl ToolCtx {
    pub fn new(audit: AuditLog) -> Self {
        Self {
            confirmed: false,
            dry_run: false,
            audit,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn confirmed(mut self) -> Self {
        self.confirmed = true;
        self
    }

    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    /// Performs the action. Implementations must check `ctx` and must not
    /// touch the system when `ctx.dry_run` is set.
    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult;

    /// Called by the registry before `call`. Default: autonomy policy.
    fn authorize(&self, ctx: &ToolCtx) -> ToolResult {
        policy::authorize_tool(self.spec().name, self.spec().risk, ctx)
    }
}

#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<&'static str, Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        crate::tools::register_all(&mut registry);
        registry
    }

    pub fn add(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.spec().name, Box::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|tool| tool.as_ref())
    }

    pub fn specs(&self) -> Vec<&ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    pub fn call(&self, name: &str, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?;
        tool.authorize(ctx)?;
        ctx.audit.record(name, tool.spec().risk, args, ctx.dry_run);
        tool.call(args, ctx)
    }
}

/// Runs a macOS CLI and returns stdout, mapping failures to `ToolError`.
pub(crate) fn run(cmd: &str, args: &[&str]) -> ToolResult {
    let output = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| ToolError::Failed(format!("{cmd}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Failed(format!(
            "{cmd} saiu com {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(Value::String(String::from_utf8_lossy(&output.stdout).to_string()))
}
