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
//! | `tools/search` | ranked, filtered tool catalog (`query`, `category`, `risk`, `limit`) |
//! | `tools/summary` | compact catalog without schemas, for progressive disclosure |
//! | `tools/list` | specs from [`Registry::specs`], including full input schemas |
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
use crate::registry::{Registry, ToolCtx, ToolError, ToolSpec};

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

    let mut reader = stdin.lock();
    loop {
        // Bounded read: `lines()` would grow a single newline-free frame until
        // the process ran out of memory.
        let mut buffer = String::new();
        let read = match io::BufRead::read_line(&mut reader, &mut buffer) {
            Ok(read) => read,
            Err(err) => {
                eprintln!("friday-mcp: erro lendo stdin: {err}");
                let _ = out.flush();
                std::process::exit(1);
            }
        };
        if read == 0 {
            break;
        }
        if buffer.len() > 8 * 1024 * 1024 {
            eprintln!("friday-mcp: frame grande demais; ignorando");
            continue;
        }
        let line = buffer.trim();
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
        "tools/search" => reply(id, search_tools(registry, &params)),
        "tools/summary" => reply(id, summary_tools(registry, &params)),
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

/// Lowercase and strip diacritics so `visão` matches `visao` and `Vóice`
/// matches `voice`. Only the repertoire used by tool names and Portuguese
/// summaries is mapped; everything else falls through to Unicode lowercase.
fn normalize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let base = match ch {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' | 'Á' | 'À' | 'Â' | 'Ã' | 'Ä' | 'Å' => {
                'a'
            }
            'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
            'ç' | 'Ç' => 'c',
            'ñ' | 'Ñ' => 'n',
            'ý' | 'ÿ' | 'Ý' | 'Ÿ' => 'y',
            'ß' => 's',
            _ => ch,
        };
        for lower in base.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

/// Coarse category derived from the tool-name prefix. Kept deterministic and
/// dependency-free: the first `_`-separated segment decides, with the
/// `codex_account` family handled before its generic `codex` sibling.
fn category_for(name: &str) -> &'static str {
    if name.starts_with("codex_account") {
        return "accounts";
    }

    let segment = name.split('_').next().unwrap_or(name);
    match segment {
        "ax" => "accessibility",
        "input" => "input",
        "app" | "apps" | "shortcut" => "apps",
        "shell" | "process" | "power" | "volume" => "system",
        "fs" | "spotlight" | "clipboard" | "keychain" => "files",
        "screen" | "vision" => "vision",
        "net" | "wifi" | "calendar" | "reminder" | "reminders" | "contact" | "contacts"
        | "home" => "network",
        "voice" | "say" => "voice",
        "knowledge" | "context" => "knowledge",
        "overlay" => "panels",
        "webhook" | "routine" | "watcher" | "watchers" => "automation",
        "assistant" | "assistants" => "agents",
        "autonomy" => "policy",
        "memory" => "memory",
        "codex" => "accounts",
        "diagnose" | "forensics" => "diagnostics",
        _ => "other",
    }
}

/// Score one tool against the normalized query words. Each word contributes
/// its best match, so multi-word queries prefer tools covering more words.
/// Returns `None` when any word is absent (an AND across words).
fn score_tool(spec: &ToolSpec, words: &[String]) -> Option<u32> {
    if words.is_empty() {
        return Some(0);
    }

    let name = normalize_text(spec.name);
    let summary = normalize_text(spec.summary);
    let mut total = 0u32;

    for word in words {
        let score = if name == *word {
            100
        } else if name.starts_with(word.as_str()) {
            70
        } else if name.contains(word.as_str()) {
            50
        } else if summary.contains(word.as_str()) {
            20
        } else {
            return None;
        };
        total += score;
    }

    Some(total)
}

fn param_filter(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(normalize_text)
        .filter(|value| !value.is_empty())
}

/// Ranked tool discovery. Pure helper shared by the dispatcher and the tests.
fn search_tools(registry: &Registry, params: &Value) -> Value {
    let query = params.get("query").and_then(Value::as_str).unwrap_or("");
    let words: Vec<String> = query
        .split_whitespace()
        .map(normalize_text)
        .filter(|word| !word.is_empty())
        .collect();

    let category = param_filter(params, "category");
    let risk = param_filter(params, "risk");
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .min(100) as usize;

    let mut matches: Vec<(u32, &ToolSpec)> = registry
        .specs()
        .into_iter()
        .filter(|spec| {
            risk.as_ref()
                .map_or(true, |wanted| normalize_text(spec.risk.label()) == *wanted)
        })
        .filter(|spec| {
            category
                .as_ref()
                .map_or(true, |wanted| category_for(spec.name) == wanted.as_str())
        })
        .filter_map(|spec| score_tool(spec, &words).map(|score| (score, spec)))
        .collect();

    // Deterministic: score first, then name so equal scores never reorder
    // between runs or platforms.
    matches.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.name.cmp(right.1.name))
    });

    let total = matches.len();
    let tools: Vec<Value> = matches
        .into_iter()
        .take(limit)
        .map(|(_, spec)| {
            json!({
                "name": spec.name,
                "description": spec.summary,
                "risk": spec.risk.label(),
                "category": category_for(spec.name),
            })
        })
        .collect();
    let count = tools.len();

    json!({ "tools": tools, "count": count, "total": total })
}

/// First 90 characters of a summary, on a character boundary.
fn short_summary(summary: &str) -> String {
    summary.chars().take(90).collect()
}

/// Compact catalog for progressive disclosure: names, risk, category and a
/// short sentence, but no input schemas. Pure helper shared by the tests.
fn summary_tools(registry: &Registry, params: &Value) -> Value {
    let category = param_filter(params, "category");
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .min(500) as usize;

    let mut specs: Vec<&ToolSpec> = registry
        .specs()
        .into_iter()
        .filter(|spec| {
            category
                .as_ref()
                .map_or(true, |wanted| category_for(spec.name) == wanted.as_str())
        })
        .collect();
    specs.sort_by_key(|spec| spec.name);

    let total = specs.len();
    let tools: Vec<Value> = specs
        .into_iter()
        .take(limit)
        .map(|spec| {
            json!({
                "name": spec.name,
                "risk": spec.risk.label(),
                "category": category_for(spec.name),
                "short": short_summary(spec.summary),
            })
        })
        .collect();
    let count = tools.len();

    json!({ "tools": tools, "count": count, "total": total })
}

fn compact_mode() -> bool {
    std::env::var_os("FRIDAY_MCP_FULL").is_none()
}

fn compact_tools() -> Value {
    json!({ "tools": [
        {
            "name": "exec",
            "description": "Executa qualquer ferramenta do catálogo Friday pelo nome. Use search para descobrir nomes.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "arguments": { "type": "object" },
                    "__confirm": { "type": "boolean" },
                    "__dry_run": { "type": "boolean" }
                }
            }
        },
        {
            "name": "search",
            "description": "Busca ferramentas por nome, descrição, categoria ou risco.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "category": { "type": "string" },
                    "risk": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }
        },
        {
            "name": "schema",
            "description": "Devolve o schema de argumentos de uma ferramenta específica.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } }
            }
        }
    ]})
}

fn tools_list(registry: &Registry) -> Value {
    if compact_mode() {
        return compact_tools();
    }
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

    // Compact mode: three meta-tools keep the prompt small while exposing the
    // whole catalog on demand.
    if compact_mode() {
        match name {
            "exec" => {
                let mut args = params.get("arguments").cloned().unwrap_or(Value::Null);
                let raw_name = args
                    .get("name")
                    .or_else(|| args.get("tool"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // Models often add the server namespace; accept both forms.
                let inner_name = raw_name
                    .strip_prefix("friday__")
                    .or_else(|| raw_name.strip_prefix("friday."))
                    .unwrap_or(&raw_name)
                    .to_string();
                let mut call_args = args
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                // Forward each flag on its own. Requiring both meant `__confirm`
                // alone was dropped (sensitive tools could never be confirmed)
                // and `__dry_run` alone was ignored (the tool really executed).
                let confirm_flag = args.get("__confirm").and_then(Value::as_bool);
                let dry_flag = args.get("__dry_run").and_then(Value::as_bool);
                if let Value::Object(map) = &mut call_args {
                    if let Some(confirm) = confirm_flag {
                        map.insert("__confirm".into(), json!(confirm));
                    }
                    if let Some(dry) = dry_flag {
                        map.insert("__dry_run".into(), json!(dry));
                    }
                }
                if registry.get(&inner_name).is_none() {
                    let suggestions: Vec<String> = registry
                        .specs()
                        .iter()
                        .filter(|spec| spec.name.contains(&inner_name) || inner_name.contains(spec.name))
                        .take(5)
                        .map(|spec| spec.name.to_string())
                        .collect();
                    return Outcome::Reply(error_response(
                        id,
                        INVALID_PARAMS,
                        format!(
                            "ferramenta desconhecida: {inner_name} (tente: {})",
                            suggestions.join(", ")
                        ),
                    ));
                }
                let forwarded = json!({ "name": inner_name, "arguments": call_args });
                let _ = &mut args;
                return tools_call(registry, Some(id), &forwarded);
            }
            "search" => {
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let payload = search_tools(registry, &args);
                return Outcome::Reply(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tool_result(&payload, false)
                }));
            }
            "schema" => {
                let wanted = params
                    .pointer("/arguments/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let found = registry.specs().into_iter().find(|spec| spec.name == wanted);
                return match found {
                    Some(spec) => {
                        let payload = json!({
                            "name": spec.name,
                            "description": spec.summary,
                            "risk": spec.risk.label(),
                            "inputSchema": spec.args
                        });
                        Outcome::Reply(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": tool_result(&payload, false)
                        }))
                    }
                    None => Outcome::Reply(error_response(id, INVALID_PARAMS, format!("ferramenta desconhecida: {wanted}"))),
                };
            }
            _ => {}
        }
    }

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
    // A dry run performs no action by contract, so it does not need the user's
    // confirmation; requiring it made `__dry_run` alone pointless.
    if risk.needs_confirmation() && !confirmed && !dry_run {
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
///
/// Large results are truncated into a valid JSON envelope so a single tool can
/// never flood the model context; the note tells the model how to narrow it.
fn tool_result(payload: &Value, is_error: bool) -> Value {
    const MAX_CHARS: usize = 24_000;
    let mut text = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
    if text.chars().count() > MAX_CHARS {
        let preview: String = text.chars().take(MAX_CHARS).collect();
        text = json!({
            "truncated": true,
            "content_chars": text.chars().count(),
            "note": "resultado grande truncado; refine a consulta ou peça menos itens",
            "preview": preview,
        })
        .to_string();
    }
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
    fn tools_list_matches_mode_for_an_empty_registry() {
        let response = answered(dispatch(
            &Registry::new(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ));
        let tools = response["result"]["tools"].as_array().unwrap();
        if compact_mode() {
            // Compact mode always exposes the three discovery meta-tools.
            assert!(tools.iter().any(|tool| tool["name"] == "exec"));
            assert!(tools.iter().any(|tool| tool["name"] == "search"));
            assert!(tools.iter().any(|tool| tool["name"] == "schema"));
        } else {
            assert!(tools.is_empty());
        }
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

    // ---------------------------------------------------------------------
    // tools/search + tools/summary
    // ---------------------------------------------------------------------

    use crate::policy::Risk;
    use crate::registry::{Tool, ToolResult};

    struct FakeTool {
        spec: ToolSpec,
    }

    impl Tool for FakeTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn call(&self, _args: &Value, _ctx: &ToolCtx) -> ToolResult {
            Ok(Value::Null)
        }
    }

    fn fake(name: &'static str, summary: &'static str, risk: Risk) -> FakeTool {
        FakeTool {
            spec: ToolSpec {
                name,
                summary,
                risk,
                args: json!({ "type": "object", "properties": {} }),
            },
        }
    }

    fn sample_registry() -> Registry {
        let mut registry = Registry::new();
        for tool in [
            fake("voice_say", "Fala um texto usando a voz do sistema.", Risk::ReadOnly),
            fake("voice_list", "Lista as vozes disponíveis no macOS.", Risk::ReadOnly),
            fake("voice_config", "Mostra a configuração de voz atual.", Risk::ReadOnly),
            fake("say", "Fala um texto pela voz padrão.", Risk::Reversible),
            fake("fs_read", "Lê um arquivo de texto.", Risk::ReadOnly),
            fake("memory_save", "Guarda uma memória durável.", Risk::Reversible),
        ] {
            registry.add(tool);
        }
        registry
    }

    fn names(value: &Value) -> Vec<&str> {
        value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn search_ranks_an_exact_name_first() {
        let result = search_tools(&sample_registry(), &json!({ "query": "voice_list" }));

        assert_eq!(result["tools"][0]["name"], json!("voice_list"));
        assert_eq!(result["count"], json!(1));
        assert_eq!(result["total"], json!(1));
    }

    #[test]
    fn search_is_accent_insensitive() {
        let result = search_tools(&sample_registry(), &json!({ "query": "vóice_sáy" }));

        assert_eq!(result["tools"][0]["name"], json!("voice_say"));
        assert_eq!(result["tools"][0]["category"], json!("voice"));
    }

    #[test]
    fn search_filters_by_category_and_risk() {
        let result = search_tools(
            &sample_registry(),
            &json!({ "query": "voice", "category": "vóice", "risk": "read", "limit": 1 }),
        );

        assert_eq!(result["count"], json!(1));
        assert_eq!(result["total"], json!(3));
        assert_eq!(result["tools"][0]["category"], json!("voice"));
    }

    #[test]
    fn summary_respects_category_and_limit() {
        let result = summary_tools(&sample_registry(), &json!({ "category": "voice", "limit": 1 }));

        assert_eq!(result["count"], json!(1));
        assert_eq!(result["total"], json!(4));
        let tool = &result["tools"][0];
        assert_eq!(tool["category"], json!("voice"));
        assert!(tool.get("inputSchema").is_none());
        assert!(tool.get("description").is_none());
        assert!(tool["short"].as_str().unwrap().chars().count() <= 90);
    }

    #[test]
    fn tools_summary_is_framed_as_json_rpc() {
        let response = answered(dispatch(
            &sample_registry(),
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/summary","params":{"limit":2}}"#,
        ));

        assert_eq!(response["jsonrpc"], json!("2.0"));
        assert_eq!(response["id"], json!(7));
        assert_eq!(response["result"]["count"], json!(2));
        assert_eq!(response["result"]["total"], json!(6));
        assert!(response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool.get("inputSchema").is_none()));
    }

    #[test]
    fn search_tools_is_framed_as_json_rpc() {
        let response = answered(dispatch(
            &sample_registry(),
            r#"{"jsonrpc":"2.0","id":"s1","method":"tools/search","params":{"query":"fs_read","limit":5}}"#,
        ));

        assert_eq!(response["id"], json!("s1"));
        assert_eq!(names(&response["result"]), vec!["fs_read"]);
    }
}
