//! `friday-mcp` — exposes Friday's tool registry over MCP stdio.
//!
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout; diagnostics on stderr.
//! Intended to be launched by `codex app-server` (or any MCP client) as an
//! stdio server. See `friday_tools::mcp::codex_config_snippet()` for wiring.

fn main() {
    friday_tools::mcp::run(friday_tools::Registry::with_defaults());
}
