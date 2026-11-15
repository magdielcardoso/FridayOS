//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Transport: newline-delimited JSON-RPC 2.0, one object per line, on
//! stdin/stdout. Nothing except protocol responses is ever written to stdout;
//! every diagnostic goes to stderr. Protocol version: `2024-11-05`.
//!
//! Implemented methods:
//!
//! | method | behaviour |
//! |---|---|
//! | `initialize` | protocol version, `tools` capability, server info |
//! | `notifications/initialized` | ignored (no response) |
//! | `ping` | empty result |
//! | `tools/list` | specs from [`Registry::specs`] |
//! | `tools/call` | runs a tool through the registry, with policy + audit |
//! | `shutdown` / `exit` | flush stdout and exit 0 |
//!
//! `tools/call` accepts two reserved arguments, stripped before the tool sees
//! them: `__confirm` (the user approved a sensitive action) and `__dry_run`
//! (report what would happen without touching the system). When a tool needs
//! confirmation and `__confirm` is missing, the call comes back as a normal
//! tool result with `isError: true` explaining how to retry — never as a
//! transport error, so the model can ask the user and try again.

use std::io::{self, BufRead, BufWriter, Write};
use std::panic::AssertUnwindSafe;

use serde_json::{json, Value};

use crate::audit::AuditLog;
use crate::registry::{Registry, ToolCtx, ToolError};

/// MCP revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

const VERSION: &str = env!("CARGO_PKG_VERSION");

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// What the loop should do after handling one line.
#[derive(Debug)]
enum Outcome {
    /// Write this response object, then keep reading.
    Reply(Value),
    /// Write this response object, flush, then exit 0 (`shutdown`).
    ReplyAndExit(Value),
    /// Notification or no-op: write nothing.
    Silent,
    /// Exit 0 immediately (`exit`, EOF).
    Exit,
}

/// Serves `registry` over stdio until stdin closes, `exit` arrives, or the
/// process is killed. Returns `!`: the only ways out are `std::process::exit`.
pub fn run(registry: Registry) -> ! {
    eprintln!(
        "friday-mcp {VERSION}: MCP {PROTOCOL_VERSION} em stdin/stdout ({} ferramentas)",
        registry.names().len()
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!("friday-mcp: erro lendo stdin: {err}");
                let _ = out.flush();
                std::process::exit(1);
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match dispatch(&registry, line) {
            Outcome::Reply(response) => write_response(&mut out, &response),
            Outcome::ReplyAndExit(response) => {
                write_response(&mut out, &response);
                let _ = out.flush();
                std::process::exit(0);
            }
            Outcome::Silent => {}
            Outcome::Exit => {
                let _ = out.flush();
                std::process::exit(0);
            }
        }
    }

    let _ = out.flush();
    std::process::exit(0)
}

/// Handles one JSON-RPC line. Pure apart from stderr diagnostics, which makes
/// it testable without a real client.
fn dispatch(registry: &Registry, line: &str) -> Outcome {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => {
            return Outcome::Reply(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("JSON inválido: {err}"),
            ));
        }
    };

    let Value::Object(request) = value else {
        return Outcome::Reply(error_response(
            Value::Null,
            INVALID_REQUEST,
            "esperado um objeto JSON-RPC".to_string(),
        ));
    };

    if let Some(version) = request.get("jsonrpc").and_then(Value::as_str) {
        if version != "2.0" {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            return Outcome::Reply(error_response(
                id,
                INVALID_REQUEST,
                format!("versão JSON-RPC não suportada: {version}"),
            ));
        }
    }

    // `id` may be a number or a string and must be echoed verbatim. A missing
    // or null id means notification: never answer those.
    let id = request.get("id").cloned().filter(|id| !id.is_null());

    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return error_reply(id, INVALID_REQUEST, "campo `method` ausente".to_string());
    };

    let params = request.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("desconhecido");
            if requested != PROTOCOL_VERSION {
                eprintln!(
                    "friday-mcp: cliente pediu protocolo {requested}, respondendo {PROTOCOL_VERSION}"
                );
            }
            reply(id, initialize_result())
        }
        "notifications/initialized" | "initialized" => Outcome::Silent,
        "ping" => reply(id, json!({})),
        "tools/list" => reply(id, tools_list(registry)),
        "tools/call" => tools_call(registry, id, &params),
        "shutdown" => match id {
            Some(id) => Outcome::ReplyAndExit(ok_response(id, json!({}))),
            None => Outcome::Exit,
        },
        "exit" => Outcome::Exit,
        other => {
            eprintln!("friday-mcp: método desconhecido `{other}`");
            error_reply(id, METHOD_NOT_FOUND, format!("método desconhecido: {other}"))
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "friday-tools",
            "version": VERSION
        },
        "instructions": "Ferramentas nativas do macOS da Friday. Ferramentas de risco \
                         sensitive/critical exigem o argumento booleano __confirm=true, e \
                         __dry_run=true pede apenas a descrição do que seria feito."
    })
}

fn tools_list(registry: &Registry) -> Value {
    let tools: Vec<Value> = registry
        .specs()
        .into_iter()
        .map(|spec| {
            let mut description = spec.summary.to_string();
            if spec.risk.needs_confirmation() {
                description.push_str(&format!(
                    " (risco {}: exige __confirm=true)",
                    spec.risk.label()
                ));
            }
            json!({
                "name": spec.name,
                "description": description,
                "inputSchema": spec.args,
            })
        })
        .collect();

    json!({ "tools": tools })
}

fn tools_call(registry: &Registry, id: Option<Value>, params: &Value) -> Outcome {
    // A tool call is always a request in practice; without an id there is
    // nowhere to deliver the result, so do not perform side effects.
    let Some(id) = id else {
        return Outcome::Silent;
    };

    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Outcome::Reply(error_response(
            id,
            INVALID_PARAMS,
            "params.name ausente ou não é string".to_string(),
        ));
    };

    let Some(tool) = registry.get(name) else {
        eprintln!("friday-mcp: ferramenta desconhecida `{name}`");
        return Outcome::Reply(error_response(
            id,
            INVALID_PARAMS,
            format!("ferramenta desconhecida: {name}"),
        ));
    };

    let mut args = params.get("arguments").cloned().unwrap_or(Value::Null);
    if args.is_null() {
        args = json!({});
    }
    if !args.is_object() {
        return Outcome::Reply(error_response(
            id,
            INVALID_PARAMS,
            "params.arguments deve ser um objeto JSON".to_string(),
        ));
    }

    let confirmed = args
        .get("__confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dry_run = args.get("__dry_run").and_then(Value::as_bool).unwrap_or(false);
    if let Value::Object(map) = &mut args {
        map.remove("__confirm");
        map.remove("__dry_run");
    }

    let risk = tool.spec().risk;
    if risk.needs_confirmation() && !confirmed {
        eprintln!("friday-mcp: `{name}` exige confirmação (risco {})", risk.label());
        let payload = json!({
            "error": "confirmation_required",
            "tool": name,
            "risk": risk.label(),
            "message": format!(
                "`{name}` é uma ação {} e precisa de confirmação explícita do usuário. \
                 Pergunte ao usuário; se ele autorizar, repita a chamada com \
                 \"__confirm\": true nos argumentos.",
                risk.label()
            ),
        });
        return Outcome::Reply(ok_response(id, tool_result(&payload, true)));
    }

    let mut ctx = ToolCtx::new(AuditLog::discover());
    if confirmed {
        ctx = ctx.confirmed();
    }
    if dry_run {
        ctx = ctx.dry_run();
    }

    eprintln!(
        "friday-mcp: tools/call {name} (confirm={confirmed}, dry_run={dry_run})"
    );

    // Tools are `&dyn Tool`, so the compiler cannot prove UnwindSafe. A panic
    // inside one tool must not take the server down with it.
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| registry.call(name, &args, &ctx)));

    let (payload, is_error) = match outcome {
        Ok(Ok(value)) => (value, false),
        Ok(Err(err)) => {
            eprintln!("friday-mcp: `{name}` falhou: {err}");
            (tool_error_payload(&err), true)
        }
        Err(_) => {
            eprintln!("friday-mcp: `{name}` entrou em pânico");
            (
                json!({
                    "error": "ferramenta entrou em pânico",
                    "kind": "panic",
                    "tool": name,
                }),
                true,
            )
        }
    };

    Outcome::Reply(ok_response(id, tool_result(&payload, is_error)))
}

/// MCP `CallToolResult`: a single text content block holding JSON.
fn tool_result(payload: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn tool_error_payload(err: &ToolError) -> Value {
    json!({
        "error": err.to_string(),
        "kind": error_kind(err),
    })
}

fn error_kind(err: &ToolError) -> &'static str {
    match err {
        ToolError::Denied(_) => "denied",
        ToolError::InvalidArgs(_) => "invalid_args",
        ToolError::Failed(_) => "failed",
        ToolError::NotFound(_) => "not_found",
    }
}

fn reply(id: Option<Value>, result: Value) -> Outcome {
    match id {
        Some(id) => Outcome::Reply(ok_response(id, result)),
        None => Outcome::Silent,
    }
}

fn error_reply(id: Option<Value>, code: i64, message: String) -> Outcome {
    match id {
        Some(id) => Outcome::Reply(error_response(id, code, message)),
        None => Outcome::Silent,
    }
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn write_response(out: &mut impl Write, response: &Value) {
    if serde_json::to_writer(&mut *out, response).is_err() {
        eprintln!("friday-mcp: resposta não serializável");
        return;
    }
    if let Err(err) = out.write_all(b"\n").and_then(|()| out.flush()) {
        eprintln!("friday-mcp: erro escrevendo stdout: {err}");
    }
}

/// The `config.toml` block (and `-c` equivalents) that makes `codex app-server`
/// start this binary as the `friday` MCP server.
///
/// The absolute path is derived from the running executable: when the caller is
/// `friday-mcp` itself that path is used, otherwise the `friday-mcp` sibling in
/// the same directory (both end up in the same `target/<profile>` folder, and in
/// the app bundle's `Contents/MacOS`).
pub fn codex_config_snippet() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| {
            if path.file_name().and_then(|name| name.to_str()) == Some("friday-mcp") {
                Some(path)
            } else {
                path.parent().map(|dir| dir.join("friday-mcp"))
            }
        })
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "/absolute/path/to/friday-mcp".to_string());

    format!(
        r#"# ~/.codex/config.toml
[mcp_servers.friday]
command = "{exe}"
# args = []          # nenhum argumento é necessário
# startup_timeout_sec = 10
# Ajuste o caminho acima se o binário estiver em outro lugar.

# Ou, sem editar o config.toml, passe a configuração na linha de comando:
#   codex -c 'mcp_servers.friday.command="{exe}"' app-server --stdio

# Notas:
# - `{exe}` fala MCP {PROTOCOL_VERSION} em JSON-RPC 2.0 delimitado por linha
#   (um objeto por linha) via stdin/stdout. Nada além de respostas do protocolo
#   vai para stdout; diagnóstico vai para stderr.
# - Nenhuma variável de ambiente é necessária; as ferramentas usam os CLIs do
#   macOS e o audit log em ~/Library/Application Support/FridayOS/audit.jsonl.
# - Ferramentas de risco sensitive/critical só executam com "__confirm": true
#   nos argumentos da chamada; "__dry_run": true apenas descreve o efeito.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(outcome: Outcome) -> Value {
        match outcome {
            Outcome::Reply(value) => value,
            other => panic!("esperava uma resposta, veio {other:?}"),
        }
    }

    #[test]
    fn initialize_advertises_protocol_and_tools_capability() {
        let response = answered(dispatch(
            &Registry::new(),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        ));

        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"]["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            json!(false)
        );
        assert_eq!(response["result"]["serverInfo"]["name"], json!("friday-tools"));
    }

    #[test]
    fn string_ids_are_echoed_verbatim() {
        let response = answered(dispatch(
            &Registry::new(),
            r#"{"jsonrpc":"2.0","id":"req-42","method":"ping"}"#,
        ));

        assert_eq!(response["id"], json!("req-42"));
        assert_eq!(response["result"], json!({}));
    }

    #[test]
    fn notifications_get_no_response() {
        assert!(matches!(
            dispatch(
                &Registry::new(),
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            ),
            Outcome::Silent
        ));
    }

    #[test]
    fn tools_list_is_empty_for_an_empty_registry() {
        let response = answered(dispatch(
            &Registry::new(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ));

        assert!(response["result"]["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let response = answered(dispatch(
            &Registry::new(),
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
        ));

        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[test]
    fn unknown_tool_is_invalid_params() {
        let response = answered(dispatch(
            &Registry::new(),
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ghost","arguments":{}}}"#,
        ));

        assert_eq!(response["error"]["code"], json!(INVALID_PARAMS));
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ghost"));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let response = answered(dispatch(&Registry::new(), "{nope"));

        assert_eq!(response["error"]["code"], json!(PARSE_ERROR));
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        assert!(matches!(
            dispatch(&Registry::new(), r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#),
            Outcome::ReplyAndExit(_)
        ));
        assert!(matches!(
            dispatch(&Registry::new(), r#"{"jsonrpc":"2.0","method":"exit"}"#),
            Outcome::Exit
        ));
    }

    #[test]
    fn codex_snippet_names_the_server_and_the_binary() {
        let snippet = codex_config_snippet();

        assert!(snippet.contains("[mcp_servers.friday]"));
        assert!(snippet.contains("command = \""));
        assert!(snippet.contains("friday-mcp"));
    }
}
