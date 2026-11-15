//! App control, AppleScript/JXA, Shortcuts and notifications.
//!
//! Everything here goes through the system binaries a human would use
//! (`osascript`, `open`, `killall`, `shortcuts`), so the layer stays auditable.
//! AppleScript output is emitted as ASCII 30/31 delimited rows and parsed in
//! Rust, which avoids hand-rolled JSON inside AppleScript.

use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Hard limit for every side effect. AppleScript scripts also get an explicit
/// `with timeout of 20 seconds` block so Script itself gives up first.
const TOOL_TIMEOUT: Duration = Duration::from_secs(20);

/// Field / row separators used to move structured data out of AppleScript.
const FIELD_SEP: char = '\u{1f}';
const ROW_SEP: char = '\u{1e}';

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

struct CmdOutput {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

/// Runs a command, feeding `stdin_data` when given, and enforces
/// [`TOOL_TIMEOUT`]. Output is drained on threads so a chatty child can never
/// deadlock on a full pipe.
fn run_capture(mut cmd: Command, stdin_data: Option<&str>) -> Result<CmdOutput, ToolError> {
    let program = cmd.get_program().to_string_lossy().into_owned();

    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Failed(format!("não consegui executar {program}: {e}")))?;

    if let Some(data) = stdin_data {
        if let Some(mut pipe) = child.stdin.take() {
            let data = data.to_string();
            thread::spawn(move || {
                let _ = pipe.write_all(data.as_bytes());
            });
        }
    }

    let out_thread = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
    });
    let err_thread = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > TOOL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ToolError::Failed(format!(
                        "{program} excedeu o tempo limite de {}s e foi encerrado",
                        TOOL_TIMEOUT.as_secs()
                    )));
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(ToolError::Failed(format!(
                    "erro ao esperar por {program}: {e}"
                )))
            }
        }
    };

    let stdout = out_thread
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = err_thread
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();

    Ok(CmdOutput {
        stdout,
        stderr,
        status,
    })
}

/// Adds a permission hint when the failure looks like a macOS TCC denial.
/// The first AppleScript run always triggers a system prompt; after a "Don't
/// Allow" the same error comes back silently.
fn tcc_hint(detail: &str) -> String {
    let lower = detail.to_lowercase();
    let denied = lower.contains("not authorized")
        || lower.contains("not allowed")
        || lower.contains("assistive access")
        || detail.contains("-1743")
        || detail.contains("-25211");
    if detail.trim().is_empty() {
        return "sem detalhes".to_string();
    }
    if denied {
        return format!(
            "{detail} — dica: autorize o Friday em Ajustes do Sistema > Privacidade e Segurança > Automação (e Acessibilidade, para controlar janelas)"
        );
    }
    detail.to_string()
}

fn ensure_success(output: CmdOutput, what: &str) -> Result<CmdOutput, ToolError> {
    if output.status.success() {
        return Ok(output);
    }
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    Err(ToolError::Failed(format!(
        "{what} falhou ({}): {}",
        output.status,
        tcc_hint(detail)
    )))
}

fn describe(err: &ToolError) -> String {
    match err {
        ToolError::Denied(msg)
        | ToolError::InvalidArgs(msg)
        | ToolError::Failed(msg)
        | ToolError::NotFound(msg) => msg.clone(),
    }
}

// ---------------------------------------------------------------------------
// AppleScript helpers
// ---------------------------------------------------------------------------

/// Escapes a value so it can be placed inside an AppleScript string literal.
/// AppleScript only understands `\"` and `\\`; newlines are legal verbatim.
fn escape_osa(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

/// A fully quoted AppleScript string literal.
fn osa_string(value: &str) -> String {
    format!("\"{}\"", escape_osa(value))
}

fn run_applescript(script: &str) -> Result<CmdOutput, ToolError> {
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(script);
    ensure_success(run_capture(cmd, None)?, "osascript")
}

fn run_jxa(script: &str) -> Result<CmdOutput, ToolError> {
    let mut cmd = Command::new("osascript");
    cmd.arg("-l").arg("JavaScript").arg("-e").arg(script);
    ensure_success(run_capture(cmd, None)?, "osascript (JXA)")
}

/// Wraps a plain statement list in a `with timeout` block. Scripts that define
/// handlers (`on run`, `using terms from`, ...) are left alone because a handler
/// cannot live inside a timeout block.
fn with_osa_timeout(script: &str) -> String {
    let trimmed = script.trim_start();
    let has_handler = trimmed.starts_with("on ")
        || trimmed.starts_with("using terms from")
        || script.contains("\non run");
    if has_handler {
        script.to_string()
    } else {
        format!("with timeout of {} seconds\n{script}\nend timeout", TOOL_TIMEOUT.as_secs())
    }
}

/// Splits the row/field delimited output produced by the scripts below.
fn split_rows(text: &str) -> Vec<Vec<String>> {
    text.trim_end_matches(['\r', '\n'])
        .split(ROW_SEP)
        .filter(|row| !row.is_empty())
        .map(|row| row.split(FIELD_SEP).map(str::to_string).collect())
        .collect()
}

fn text_field(fields: &[String], index: usize) -> String {
    fields.get(index).cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    match args.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.as_str()),
        Some(Value::String(_)) => Err(ToolError::invalid(format!("\"{key}\" não pode ser vazio"))),
        Some(_) => Err(ToolError::invalid(format!("\"{key}\" deve ser uma string"))),
        None => Err(ToolError::invalid(format!(
            "falta o argumento obrigatório \"{key}\""
        ))),
    }
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(ToolError::invalid(format!("\"{key}\" deve ser uma string"))),
    }
}

fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::invalid(format!("\"{key}\" deve ser booleano"))),
    }
}

/// Keeps values from being interpreted as CLI flags by `open`/`killall`/etc.
fn reject_flag_like(value: &str, key: &str) -> Result<(), ToolError> {
    if value.starts_with('-') {
        return Err(ToolError::invalid(format!(
            "\"{key}\" não pode começar com '-'"
        )));
    }
    Ok(())
}

fn dry_run(would: Value) -> ToolResult {
    Ok(json!({ "dry_run": true, "would": would }))
}

fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// `open` with a proper argument vector (never through a shell).
fn open_with(args: &[&str]) -> Result<(), ToolError> {
    let mut cmd = Command::new("open");
    cmd.args(args);
    ensure_success(run_capture(cmd, None)?, "open")?;
    Ok(())
}

fn has_url_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    if scheme.is_empty() || scheme.len() > 32 {
        return false;
    }
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

// ---------------------------------------------------------------------------
// Tool wrapper
// ---------------------------------------------------------------------------

struct AppsTool {
    spec: ToolSpec,
    handler: fn(&Value, &ToolCtx) -> ToolResult,
}

impl Tool for AppsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        (self.handler)(args, ctx)
    }
}

fn tool(
    name: &'static str,
    summary: &'static str,
    risk: Risk,
    args: Value,
    handler: fn(&Value, &ToolCtx) -> ToolResult,
) -> AppsTool {
    AppsTool {
        spec: ToolSpec {
            name,
            summary,
            risk,
            args,
        },
        handler,
    }
}

// ---------------------------------------------------------------------------
// AppleScript sources
// ---------------------------------------------------------------------------

const APPS_RUNNING_SCRIPT: &str = r#"
tell application "System Events"
    set fieldSep to (ASCII character 31)
    set rowSep to (ASCII character 30)
    set rows to {}
    repeat with p in (every application process whose background only is false)
        set appName to ""
        set bundleIdText to ""
        set pidText to ""
        try
            set appName to name of p as text
        end try
        try
            set rawId to bundle identifier of p
            if rawId is not missing value then set bundleIdText to rawId as text
        end try
        try
            set rawPid to unix id of p
            if rawPid is not missing value then set pidText to rawPid as text
        end try
        set end of rows to (appName & fieldSep & bundleIdText & fieldSep & pidText)
    end repeat
    set AppleScript's text item delimiters to rowSep
    set joined to rows as text
    set AppleScript's text item delimiters to ""
    return joined
end tell
"#;

const FRONTMOST_SCRIPT: &str = r#"
tell application "System Events"
    set p to first application process whose frontmost is true
    set appName to name of p as text
    set bundleIdText to ""
    try
        set rawId to bundle identifier of p
        if rawId is not missing value then set bundleIdText to rawId as text
    end try
    return appName & (ASCII character 31) & bundleIdText
end tell
"#;

// ---------------------------------------------------------------------------
// Read-only tools
// ---------------------------------------------------------------------------

fn h_apps_running(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let output = run_applescript(APPS_RUNNING_SCRIPT)?;
    let apps: Vec<Value> = split_rows(&output.stdout)
        .into_iter()
        .filter_map(|fields| {
            let name = text_field(&fields, 0);
            if name.is_empty() {
                return None;
            }
            let bundle_id = text_field(&fields, 1);
            let pid = fields
                .get(2)
                .and_then(|text| text.trim().parse::<i64>().ok());
            Some(json!({
                "name": name,
                "bundle_id": if bundle_id.is_empty() { Value::Null } else { Value::String(bundle_id) },
                "pid": pid,
            }))
        })
        .collect();
    Ok(Value::Array(apps))
}

fn h_app_frontmost(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let output = run_applescript(FRONTMOST_SCRIPT)?;
    let fields = split_rows(&output.stdout);
    let fields = fields.first().cloned().unwrap_or_default();
    let name = text_field(&fields, 0);
    if name.is_empty() {
        return Err(ToolError::failed("nenhum aplicativo em primeiro plano"));
    }
    let bundle_id = text_field(&fields, 1);
    Ok(json!({
        "name": name,
        "bundle_id": if bundle_id.is_empty() { Value::Null } else { Value::String(bundle_id) },
    }))
}

fn h_app_windows(args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let name = required_str(args, "name")?;

    let script = format!(
        r#"
tell application "System Events"
    set targetProc to missing value
    try
        if exists application process {app} then set targetProc to application process {app}
    end try
    if targetProc is missing value then
        try
            set targetProc to first application process whose bundle identifier is {app}
        end try
    end if
    if targetProc is missing value then
        error "FRIDAY_APP_NOT_RUNNING"
    end if
    tell targetProc
        set fieldSep to (ASCII character 31)
        set rowSep to (ASCII character 30)
        set rows to {{}}
        repeat with w in windows
            set windowTitle to ""
            try
                set windowTitle to name of w as text
            end try
            set px to 0
            set py to 0
            set sw to 0
            set sh to 0
            try
                set pos to position of w
                set px to item 1 of pos
                set py to item 2 of pos
            end try
            try
                set sz to size of w
                set sw to item 1 of sz
                set sh to item 2 of sz
            end try
            set end of rows to (windowTitle & fieldSep & (px as text) & fieldSep & (py as text) & fieldSep & (sw as text) & fieldSep & (sh as text))
        end repeat
        set AppleScript's text item delimiters to rowSep
        set joined to rows as text
        set AppleScript's text item delimiters to ""
        return joined
    end tell
end tell
"#,
        app = osa_string(name)
    );

    let output = match run_applescript(&with_osa_timeout(&script)) {
        Ok(output) => output,
        Err(err) => {
            if describe(&err).contains("FRIDAY_APP_NOT_RUNNING") {
                return Err(ToolError::failed(format!(
                    "o aplicativo \"{name}\" não está em execução"
                )));
            }
            return Err(err);
        }
    };

    let windows: Vec<Value> = split_rows(&output.stdout)
        .into_iter()
        .enumerate()
        .map(|(index, fields)| {
            let bounds = |offset: usize| -> i64 {
                fields
                    .get(offset)
                    .and_then(|text| text.trim().parse::<i64>().ok())
                    .unwrap_or(0)
            };
            json!({
                "index": index + 1,
                "title": text_field(&fields, 0),
                "bounds": {
                    "x": bounds(1),
                    "y": bounds(2),
                    "width": bounds(3),
                    "height": bounds(4),
                },
            })
        })
        .collect();

    Ok(json!({ "app": name, "windows": windows }))
}

fn h_shortcut_list(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let mut cmd = Command::new("shortcuts");
    cmd.arg("list");
    let output = ensure_success(run_capture(cmd, None)?, "shortcuts list")?;

    let shortcuts: Vec<Value> = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| Value::String(line.to_string()))
        .collect();

    Ok(json!({ "count": shortcuts.len(), "shortcuts": shortcuts }))
}

// ---------------------------------------------------------------------------
// Reversible tools
// ---------------------------------------------------------------------------

fn h_app_activate(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let name = required_str(args, "name")?;
    reject_flag_like(name, "name")?;

    if ctx.dry_run {
        return dry_run(json!({ "action": "app_activate", "name": name }));
    }

    let by_name = format!(
        "with timeout of {} seconds\n    tell application {} to activate\nend timeout",
        TOOL_TIMEOUT.as_secs(),
        osa_string(name)
    );
    let first_error = match run_applescript(&by_name) {
        Ok(_) => {
            return Ok(json!({ "ok": true, "app": name, "method": "name" }));
        }
        Err(err) => describe(&err),
    };

    // Fallback: some apps are only addressable by bundle id.
    let by_id = format!(
        "with timeout of {} seconds\n    tell application id {} to activate\nend timeout",
        TOOL_TIMEOUT.as_secs(),
        osa_string(name)
    );
    match run_applescript(&by_id) {
        Ok(_) => Ok(json!({ "ok": true, "app": name, "method": "bundle_id" })),
        Err(_) => Err(ToolError::failed(format!(
            "não consegui ativar \"{name}\": {first_error}"
        ))),
    }
}

fn h_app_quit(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let name = required_str(args, "name")?;
    let force = optional_bool(args, "force")?.unwrap_or(false);
    reject_flag_like(name, "name")?;

    if ctx.dry_run {
        return dry_run(json!({
            "action": if force { "app_quit_force" } else { "app_quit" },
            "name": name,
            "force": force,
        }));
    }

    if force {
        // Direct exec, no shell: the name is one argv element, so nothing can
        // be injected or word-split.
        let mut cmd = Command::new("killall");
        cmd.arg("-9").arg(name);
        let output = run_capture(cmd, None)?;
        if !output.status.success() {
            let stderr = output.stderr.trim();
            let message = if stderr.is_empty() || stderr.contains("No matching processes") {
                format!("nenhum processo em execução chamado \"{name}\"")
            } else {
                tcc_hint(stderr)
            };
            return Err(ToolError::failed(message));
        }
        return Ok(json!({ "ok": true, "app": name, "force": true }));
    }

    // Checking first avoids AppleScript launching a stopped app just to tell it
    // to quit, and lets us accept either a display name or a bundle id.
    let probe = format!(
        r#"
tell application "System Events"
    set matchedByName to false
    set matchedById to false
    try
        if exists application process {app} then set matchedByName to true
    end try
    if not matchedByName then
        try
            if exists (first application process whose bundle identifier is {app}) then set matchedById to true
        end try
    end if
    return (matchedByName as text) & "/" & (matchedById as text)
end tell
"#,
        app = osa_string(name)
    );

    let probe = run_applescript(&probe)?;
    let matched = probe.stdout.trim();
    let by_name = matched.starts_with("true");
    let by_id = matched.ends_with("true");

    if !by_name && !by_id {
        return Ok(json!({
            "ok": true,
            "app": name,
            "force": false,
            "was_running": false,
        }));
    }

    // Built separately: `tell application id "..."` is resolved when the script
    // is compiled, so it may only appear when the target really is a bundle id.
    let quit = if by_name {
        format!(
            "with timeout of {} seconds\n    tell application {} to quit\nend timeout",
            TOOL_TIMEOUT.as_secs(),
            osa_string(name)
        )
    } else {
        format!(
            "with timeout of {} seconds\n    tell application id {} to quit\nend timeout",
            TOOL_TIMEOUT.as_secs(),
            osa_string(name)
        )
    };
    run_applescript(&quit)?;

    Ok(json!({
        "ok": true,
        "app": name,
        "force": false,
        "was_running": true,
        "method": if by_name { "name" } else { "bundle_id" },
    }))
}

fn h_app_launch(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let target = required_str(args, "path_or_bundle")?;
    reject_flag_like(target, "path_or_bundle")?;

    if ctx.dry_run {
        return dry_run(json!({ "action": "app_launch", "target": target }));
    }

    let lowered = target.to_lowercase();
    let path_like = target.starts_with('/')
        || target.starts_with("~/")
        || lowered.ends_with(".app")
        || expanded_exists(target);

    if path_like {
        let path = expand_tilde(target);
        match open_with(&[path.as_str()]) {
            Ok(()) => Ok(json!({ "ok": true, "launched": target, "how": "path" })),
            Err(first) => match open_with(&["-a", path.as_str()]) {
                Ok(()) => Ok(json!({ "ok": true, "launched": target, "how": "path_app" })),
                Err(_) => Err(first),
            },
        }
    } else {
        match open_with(&["-a", target]) {
            Ok(()) => Ok(json!({ "ok": true, "launched": target, "how": "app_name" })),
            Err(first) => match open_with(&[target]) {
                Ok(()) => Ok(json!({ "ok": true, "launched": target, "how": "direct" })),
                Err(_) => Err(first),
            },
        }
    }
}

fn expanded_exists(target: &str) -> bool {
    std::path::Path::new(&expand_tilde(target)).exists()
}

fn h_open_url(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let url = required_str(args, "url")?;
    if !has_url_scheme(url) {
        return Err(ToolError::invalid(format!(
            "\"{url}\" não parece uma URL com esquema (ex.: https://example.com)"
        )));
    }

    if ctx.dry_run {
        return dry_run(json!({ "action": "open_url", "url": url }));
    }

    open_with(&[url])?;
    Ok(json!({ "ok": true, "url": url }))
}

fn h_shortcut_run(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let name = required_str(args, "name")?;
    reject_flag_like(name, "name")?;
    let input = optional_str(args, "input")?;

    if ctx.dry_run {
        return dry_run(json!({
            "action": "shortcut_run",
            "name": name,
            "with_input": input.is_some(),
        }));
    }

    let mut cmd = Command::new("shortcuts");
    cmd.arg("run").arg(name);
    let output = ensure_success(run_capture(cmd, input)?, "shortcuts run")?;

    Ok(json!({
        "ok": true,
        "shortcut": name,
        "stdout": output.stdout.trim_end().to_string(),
        "stderr": output.stderr.trim_end().to_string(),
    }))
}

fn h_notification_send(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let title = required_str(args, "title")?;
    let body = required_str(args, "body")?;

    if ctx.dry_run {
        return dry_run(json!({
            "action": "notification_send",
            "title": title,
            "body": body,
        }));
    }

    let script = format!(
        "with timeout of {} seconds\n    display notification {} with title {}\nend timeout",
        TOOL_TIMEOUT.as_secs(),
        osa_string(body),
        osa_string(title)
    );
    run_applescript(&script)?;
    Ok(json!({ "ok": true, "title": title }))
}

// ---------------------------------------------------------------------------
// Sensitive tools
// ---------------------------------------------------------------------------

fn h_applescript_run(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let script = required_str(args, "script")?;

    if ctx.dry_run {
        return dry_run(json!({ "action": "applescript_run", "script": script }));
    }

    let output = run_applescript(&with_osa_timeout(script))?;
    Ok(json!({
        "stdout": output.stdout.trim_end().to_string(),
        "stderr": output.stderr.trim_end().to_string(),
    }))
}

fn h_jxa_run(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let script = required_str(args, "script")?;

    if ctx.dry_run {
        return dry_run(json!({ "action": "jxa_run", "script": script }));
    }

    let output = run_jxa(script)?;
    Ok(json!({
        "stdout": output.stdout.trim_end().to_string(),
        "stderr": output.stderr.trim_end().to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn no_args() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn str_arg(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "apps_running",
        "Lista os aplicativos em execução (nome, bundle id e pid).",
        Risk::ReadOnly,
        no_args(),
        h_apps_running,
    ));

    registry.add(tool(
        "app_frontmost",
        "Mostra o aplicativo em primeiro plano (nome e bundle id).",
        Risk::ReadOnly,
        no_args(),
        h_app_frontmost,
    ));

    registry.add(tool(
        "app_windows",
        "Lista as janelas de um aplicativo, com título, posição e tamanho.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {
                "name": str_arg("nome do aplicativo, ex.: Safari"),
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
        h_app_windows,
    ));

    registry.add(tool(
        "app_activate",
        "Traz um aplicativo para a frente, ativando-o pelo nome ou bundle id.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "name": str_arg("nome do aplicativo ou bundle id, ex.: Safari"),
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
        h_app_activate,
    ));

    registry.add(tool(
        "app_quit",
        "Encerra um aplicativo; com force, mata o processo à força.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "name": str_arg("nome do aplicativo, ex.: Safari"),
                "force": {
                    "type": "boolean",
                    "description": "mata o processo com SIGKILL em vez de pedir para sair",
                    "default": false,
                },
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
        h_app_quit,
    ));

    registry.add(tool(
        "app_launch",
        "Abre um aplicativo pelo nome/bundle id ou pelo caminho do .app.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "path_or_bundle": str_arg("nome do app, bundle id ou caminho até o .app"),
            },
            "required": ["path_or_bundle"],
            "additionalProperties": false,
        }),
        h_app_launch,
    ));

    registry.add(tool(
        "open_url",
        "Abre uma URL no navegador ou no aplicativo padrão do sistema.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "url": str_arg("URL completa com esquema, ex.: https://example.com"),
            },
            "required": ["url"],
            "additionalProperties": false,
        }),
        h_open_url,
    ));

    registry.add(tool(
        "shortcut_list",
        "Lista os atalhos disponíveis no app Atalhos.",
        Risk::ReadOnly,
        no_args(),
        h_shortcut_list,
    ));

    registry.add(tool(
        "shortcut_run",
        "Executa um atalho do app Atalhos, opcionalmente passando uma entrada.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "name": str_arg("nome exato do atalho"),
                "input": str_arg("texto enviado à entrada do atalho (stdin)"),
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
        h_shortcut_run,
    ));

    registry.add(tool(
        "notification_send",
        "Mostra uma notificação do sistema com título e corpo.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "title": str_arg("título da notificação"),
                "body": str_arg("corpo da notificação"),
            },
            "required": ["title", "body"],
            "additionalProperties": false,
        }),
        h_notification_send,
    ));

    registry.add(tool(
        "applescript_run",
        "Executa um script AppleScript arbitrário e devolve stdout/stderr (requer confirmação).",
        Risk::Sensitive,
        json!({
            "type": "object",
            "properties": {
                "script": str_arg("código AppleScript a executar"),
            },
            "required": ["script"],
            "additionalProperties": false,
        }),
        h_applescript_run,
    ));

    registry.add(tool(
        "jxa_run",
        "Executa JavaScript for Automation (JXA) arbitrário e devolve stdout/stderr (requer confirmação).",
        Risk::Sensitive,
        json!({
            "type": "object",
            "properties": {
                "script": str_arg("código JavaScript for Automation a executar"),
            },
            "required": ["script"],
            "additionalProperties": false,
        }),
        h_jxa_run,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_applescript_literals() {
        assert_eq!(escape_osa(r#"say "hi" \ bye"#), r#"say \"hi\" \\ bye"#);
        assert_eq!(osa_string("Safari"), "\"Safari\"");
        assert_eq!(escape_osa("linha\nnova"), "linha\nnova");
    }

    #[test]
    fn splits_delimited_rows() {
        let raw = format!("Safari{FIELD_SEP}com.apple.Safari{FIELD_SEP}123{ROW_SEP}Notes{FIELD_SEP}{FIELD_SEP}456\n");
        let rows = split_rows(&raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "Safari");
        assert_eq!(rows[1][1], "");
        assert!(split_rows("").is_empty());
    }

    #[test]
    fn recognises_url_schemes() {
        assert!(has_url_scheme("https://example.com"));
        assert!(has_url_scheme("mailto:a@b.c"));
        assert!(!has_url_scheme("example.com"));
        assert!(!has_url_scheme("-nope:foo"));
        assert!(!has_url_scheme("://x"));
    }
}
