//! Friday's hands and eyes.
//!
//! Every capability is a [`Tool`] with an explicit risk level. Tools are
//! deliberately boring: they shell out to macOS CLIs or tiny Swift helpers so
//! the layer stays auditable and easy to sandbox later.
//!
//! The same registry powers the in-app assistant and the MCP server that Codex
//! consumes, so there is exactly one implementation of "what Friday can do".

pub mod audit;
pub mod policy;
pub mod mcp;
pub mod registry;
pub mod swift;
pub mod tools;
pub mod voice_engine;
pub mod delivery;
pub mod decision;
pub mod watcher_events;

pub use audit::AuditLog;
pub use policy::{Autonomy, Risk};
pub use registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};
